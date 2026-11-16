//! Desktop integration for blurt: global hotkeys, the system tray, and the
//! call as the system sees it (universal mute), behind platform-neutral
//! types so the app doesn't see which desktop API is underneath.
//!
//! Linux uses the XDG GlobalShortcuts portal (through ashpd), so the
//! desktop (KDE, GNOME 48+, Hyprland...) owns the key bindings: it asks the
//! user to approve them and lets them rebind. The tray icon is a
//! StatusNotifierItem (through ksni). Both share one D-Bus library (zbus).
//!
//! Windows has the tray, in the notification area (through windows-sys),
//! and universal mute: Windows 11's taskbar microphone button and
//! Win+Alt+K (through WinRT's VoIP calls). Global hotkeys there are still
//! to come.

pub mod call;
pub mod hotkeys;
pub mod tray;

/// Something the user can trigger from a global hotkey or the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Action {
    ToggleMute,
    ToggleDeafen,
    PushToTalk,
    /// Show the window, or hide it if it's showing.
    ShowWindow,
    Quit,
}
