//! The Windows tray: a notification area icon (`Shell_NotifyIconW`).
//!
//! The icon belongs to a hidden window on a thread of its own, which runs
//! the Win32 message loop: the shell reports clicks to that window, and
//! the menu runs its own loop there. The async side ([`run`]) hands it new
//! states and wakes it with a message. The window is an ordinary top-level
//! one, never shown, rather than message-only: only top-level windows hear
//! `TaskbarCreated`, which is how Explorer restarting is noticed.

use std::cell::{Cell, RefCell};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex, PoisonError};

use futures_channel::{mpsc, oneshot};
use futures_util::StreamExt;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS,
    DeleteObject,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, GetSystemMetricsForDpi};
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION,
    NIN_SELECT, NOTIFYICON_VERSION_4, NOTIFYICONDATAW, Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, FindWindowW, GWLP_USERDATA, GetMessageW,
    GetWindowLongPtrW, HICON, ICONINFO, MF_CHECKED, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW, SM_CXSMICON,
    SetForegroundWindow, SetWindowLongPtrW, TPM_BOTTOMALIGN, TPM_NONOTIFY, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, TrackPopupMenuEx, TranslateMessage, WM_APP, WM_CLOSE, WM_CONTEXTMENU,
    WM_DESTROY, WM_DISPLAYCHANGE, WM_NULL, WM_SETTINGCHANGE, WNDCLASSW, WS_OVERLAPPED,
};

use super::{Event, Item, State, bgra, menu, pick, tooltip};
use crate::Action;

/// The shell reports clicks on the icon with this.
const WM_NOTIFY_ICON: u32 = WM_APP + 1;
/// The async side set a new state.
const WM_UPDATE: u32 = WM_APP + 2;
/// Enter or Space on the focused icon (NIN_SELECT | NINF_KEY); windows-sys
/// doesn't name it.
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
/// blurt has the one icon.
const ICON_ID: u32 = 1;

pub async fn run(
    _app_id: &str,
    state: State,
    events: &mpsc::UnboundedSender<Event>,
    mut inbox: mpsc::UnboundedReceiver<State>,
) -> Result<(), String> {
    let shared = Arc::new(Mutex::new(state));
    let (ready, started) = oneshot::channel();
    std::thread::Builder::new()
        .name("blurt-tray".into())
        .spawn({
            let shared = shared.clone();
            let events = events.clone();
            move || thread(shared, events, ready)
        })
        .map_err(|e| e.to_string())?;
    // The HWND crosses threads as a number: posting to it is thread-safe.
    let hwnd = started
        .await
        .map_err(|_| "the tray thread stopped".to_owned())??;
    let post = |msg| unsafe { PostMessageW(hwnd as HWND, msg, 0, 0) } != 0;
    let _ = events.unbounded_send(Event::Shown);
    while let Some(mut state) = inbox.next().await {
        // Only the latest matters.
        while let Ok(newer) = inbox.try_recv() {
            state = newer;
        }
        *shared.lock().unwrap_or_else(PoisonError::into_inner) = state;
        if !post(WM_UPDATE) {
            return Err("the tray service stopped".into());
        }
    }
    // The app stopped listening: take the icon down.
    post(WM_CLOSE);
    Ok(())
}

/// What the window's procedure works with. It's only ever borrowed
/// shared, since the menu's loop calls the procedure again from inside it.
struct Tray {
    hwnd: HWND,
    state: Arc<Mutex<State>>,
    events: mpsc::UnboundedSender<Event>,
    /// Explorer broadcasts this when it (re)starts.
    taskbar_created: u32,
    /// The icon the shell shows, and the image it was made from.
    icon: Cell<HICON>,
    image: RefCell<Option<super::Image>>,
    /// Taken down for good: blurt is quitting.
    gone: Cell<bool>,
}

fn thread(
    state: Arc<Mutex<State>>,
    events: mpsc::UnboundedSender<Event>,
    ready: oneshot::Sender<Result<usize, String>>,
) {
    let class = wide("blurt-tray");
    unsafe {
        let instance = GetModuleHandleW(null());
        let wc = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        // Fails if it's registered already, which is fine: creating the
        // window below is what has to work.
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            wide("blurt").as_ptr(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            instance,
            null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err(format!(
                "could not create the tray's window: {}",
                std::io::Error::last_os_error()
            )));
            return;
        }
        let tray = Box::into_raw(Box::new(Tray {
            hwnd,
            state,
            events,
            taskbar_created: RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()),
            icon: Cell::new(null_mut()),
            image: RefCell::new(None),
            gone: Cell::new(false),
        }));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, tray as isize);
        if !(*tray).add() {
            let _ = ready.send(Err("the notification area refused the icon".into()));
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DestroyWindow(hwnd);
            drop(Box::from_raw(tray));
            return;
        }
        let _ = ready.send(Ok(hwnd as usize));
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        // Normally the window is gone by now; if the loop failed, it
        // isn't, so detach it before letting go of what it points to.
        (*tray).remove();
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(tray));
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let tray = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const Tray;
    // Set straight after the window is created; until then, the defaults.
    let Some(tray) = (unsafe { tray.as_ref() }) else {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    };
    match msg {
        WM_NOTIFY_ICON => {
            // NOTIFYICON_VERSION_4: the event in the low word of lParam,
            // and where to put a menu in wParam.
            match lparam as u32 & 0xffff {
                NIN_SELECT | NIN_KEYSELECT => tray.send(Action::ShowWindow),
                WM_CONTEXTMENU => {
                    let x = (wparam & 0xffff) as u16 as i16;
                    let y = ((wparam >> 16) & 0xffff) as u16 as i16;
                    tray.show_menu(x.into(), y.into());
                }
                _ => {}
            }
            0
        }
        WM_UPDATE => {
            tray.notify(NIM_MODIFY);
            0
        }
        m if m == tray.taskbar_created => {
            // Explorer restarted, and the icon went with it.
            log::info!("The taskbar is back: showing the tray icon again");
            tray.add();
            0
        }
        WM_DISPLAYCHANGE | WM_SETTINGCHANGE => {
            // Perhaps a new scale: the icon may want another size.
            tray.notify(NIM_MODIFY);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_CLOSE => {
            tray.remove();
            unsafe { DestroyWindow(hwnd) };
            0
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            0
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

impl Tray {
    fn send(&self, action: Action) {
        let _ = self.events.unbounded_send(Event::Action(action));
    }

    fn state(&self) -> State {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn data(&self) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: ICON_ID,
            ..Default::default()
        }
    }

    /// Puts the icon in the notification area.
    fn add(&self) -> bool {
        if self.gone.get() {
            return false;
        }
        if !self.notify(NIM_ADD) {
            return false;
        }
        let mut data = self.data();
        data.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        unsafe { Shell_NotifyIconW(NIM_SETVERSION, &data) != 0 }
    }

    /// Adds the icon or brings it up to date: its image and tooltip.
    fn notify(&self, message: u32) -> bool {
        if self.gone.get() {
            return false;
        }
        let state = self.state();
        let fresh = self.fresh_icon(&state);
        let mut data = self.data();
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
        data.uCallbackMessage = WM_NOTIFY_ICON;
        data.hIcon = fresh.unwrap_or(self.icon.get());
        data.szTip = tooltip(&state.status);
        let done = unsafe { Shell_NotifyIconW(message, &data) } != 0;
        // The shell has its own copy now: the old one can go.
        if let Some(fresh) = fresh {
            let old = self.icon.replace(fresh);
            if !old.is_null() {
                unsafe { DestroyIcon(old) };
            }
        }
        done
    }

    /// Takes the icon down, for good.
    fn remove(&self) {
        if self.gone.replace(true) {
            return;
        }
        unsafe { Shell_NotifyIconW(NIM_DELETE, &self.data()) };
    }

    /// A new icon, if the image the state calls for (at the size the
    /// taskbar wants now) isn't the one showing.
    fn fresh_icon(&self, state: &State) -> Option<HICON> {
        let image = pick(&state.icon, icon_size())?;
        if self.image.borrow().as_ref() == Some(image) {
            return None;
        }
        let icon = create_icon(image)?;
        *self.image.borrow_mut() = Some(image.clone());
        Some(icon)
    }

    fn show_menu(&self, x: i32, y: i32) {
        let items = menu(&self.state());
        let chosen = unsafe {
            let popup = CreatePopupMenu();
            if popup.is_null() {
                return;
            }
            for (i, item) in items.iter().enumerate() {
                let id = i + 1;
                match item {
                    Item::Button { label, .. } => {
                        AppendMenuW(popup, MF_STRING, id, wide(label).as_ptr());
                    }
                    Item::Check {
                        label,
                        checked,
                        enabled,
                        ..
                    } => {
                        let mut flags = MF_STRING;
                        if *checked {
                            flags |= MF_CHECKED;
                        }
                        if !*enabled {
                            flags |= MF_GRAYED;
                        }
                        AppendMenuW(popup, flags, id, wide(label).as_ptr());
                    }
                    Item::Separator => {
                        AppendMenuW(popup, MF_SEPARATOR, 0, null());
                    }
                }
            }
            // Without these two, the menu doesn't close when you click
            // somewhere else (see TrackPopupMenu's documentation).
            SetForegroundWindow(self.hwnd);
            let chosen = TrackPopupMenuEx(
                popup,
                TPM_RETURNCMD | TPM_NONOTIFY | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                x,
                y,
                self.hwnd,
                null(),
            );
            PostMessageW(self.hwnd, WM_NULL, 0, 0);
            DestroyMenu(popup);
            chosen
        };
        let action = (chosen as usize)
            .checked_sub(1)
            .and_then(|i| items.get(i))
            .and_then(|item| match item {
                Item::Button { action, .. } | Item::Check { action, .. } => Some(*action),
                Item::Separator => None,
            });
        if let Some(action) = action {
            // blurt exits without tearing the tray down, and an icon left
            // behind lingers in the notification area until hovered.
            if action == Action::Quit {
                self.remove();
            }
            self.send(action);
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let icon = self.icon.get();
        if !icon.is_null() {
            unsafe { DestroyIcon(icon) };
        }
    }
}

/// The small icon's size at the taskbar's scale. The taskbar's own DPI
/// follows its monitor; the system DPI is only the scale at sign-in.
fn icon_size() -> u32 {
    let size = unsafe {
        let taskbar = FindWindowW(wide("Shell_TrayWnd").as_ptr(), null());
        let dpi = if taskbar.is_null() {
            0
        } else {
            GetDpiForWindow(taskbar)
        };
        let dpi = if dpi == 0 { GetDpiForSystem() } else { dpi };
        GetSystemMetricsForDpi(SM_CXSMICON, dpi)
    };
    size.max(16) as u32
}

/// An icon from `image`: 32-bit colour with alpha, and an empty mask
/// (which a colour bitmap with alpha needs, but doesn't use).
fn create_icon(image: &super::Image) -> Option<HICON> {
    let (width, height) = (image.width as i32, image.height as i32);
    let header = BITMAPINFOHEADER {
        biSize: size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: width,
        // Negative: rows top to bottom, as the image has them.
        biHeight: -height,
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..Default::default()
    };
    let info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    let pixels = bgra(image);
    // Monochrome rows are padded to 16 bits.
    let mask = vec![0u8; (image.width.div_ceil(16) * 2 * image.height) as usize];
    unsafe {
        let mut bits = null_mut();
        let color = CreateDIBSection(null_mut(), &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if color.is_null() || bits.is_null() {
            return None;
        }
        std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast(), pixels.len());
        let mask = CreateBitmap(width, height, 1, 1, mask.as_ptr().cast());
        let info = ICONINFO {
            fIcon: 1,
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&info);
        // The icon keeps copies of both bitmaps.
        DeleteObject(color);
        if !mask.is_null() {
            DeleteObject(mask);
        }
        (!icon.is_null()).then_some(icon)
    }
}

/// `s` as a NUL-terminated UTF-16 string.
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain([0]).collect()
}
