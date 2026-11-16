//! blurt: a small, fast, native Mumble client.
// On Windows, a GUI program: no console window opens alongside it.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod config;
mod html;
mod icons;
mod rich;
mod room;
mod session;
#[cfg(test)]
mod snapshots;
mod sounds;
mod talk;
mod theme;
mod transcript;
mod tray_icon;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use blurt_audio::EngineConfig;
use blurt_platform::{Action, call, hotkeys, tray};
use config::{Config, Server};
use iced::widget::{self, column, container};
use iced::{Element, Fill, Size, Subscription, Task, keyboard, window};
use mumble_protocol::client::{ConnectionOptions, Fingerprint};
use mumble_protocol::state::MumbleState;
use room::Room;
use session::Session;
use sounds::Sound;
use theme::{Accent, Mode, Tokens};
use ui::pages::{self, Editor, Field};
use ui::voice::{self, progress};
use ui::{Layout, Page, Pivot, chrome};

fn main() -> iced::Result {
    let mut log =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    // A Windows GUI program has no console: the log goes to a file instead.
    #[cfg(windows)]
    if let Some(file) = log_file() {
        log.target(env_logger::Target::Pipe(Box::new(file)));
    }
    log.format_timestamp_millis().init();
    log::info!("blurt {} starting", env!("CARGO_PKG_VERSION"));
    // The renderer is picked when iced starts, from the setting as saved.
    let rich = config::dir()
        .and_then(|dir| config::load(&dir).ok())
        .map(|c| c.appearance.rich_visuals)
        .unwrap_or_default();
    rich::choose_renderer(rich.wanted());
    // A daemon outlives its windows: with a tray, closing the window
    // leaves blurt running there.
    iced::daemon(Blurt::new, Blurt::update, Blurt::view)
        .title(Blurt::title)
        .theme(Blurt::theme)
        .font(theme::FONTS[0])
        .font(theme::FONTS[1])
        .default_font(theme::ui(iced::font::Weight::Normal))
        .subscription(Blurt::subscription)
        .run()
}

/// `%LOCALAPPDATA%\blurt\data\blurt.log`, fresh each run.
#[cfg(windows)]
fn log_file() -> Option<std::fs::File> {
    let dirs = directories::ProjectDirs::from("", "", "blurt")?;
    let dir = dirs.data_local_dir();
    std::fs::create_dir_all(dir).ok()?;
    std::fs::File::create(dir.join("blurt.log")).ok()
}

/// Opens blurt's window: at startup, and again from the tray. Its id
/// counts as the window straight away, before it has opened, so nothing
/// that arrives meanwhile (such as the tray failing, which is instant
/// without one) opens a second.
fn open_window() -> (window::Id, Task<Message>) {
    let (id, open) = window::open(window::Settings {
        size: WINDOW,
        min_size: Some(Size::new(360.0, 480.0)),
        // blurt draws its own title bar.
        decorations: false,
        #[cfg(windows)]
        icon: window_icon(),
        ..window::Settings::default()
    });
    (id, open.map(Message::WindowOpened))
}

/// Windows shows a window's own icon in the taskbar and Alt+Tab. The
/// tray's drawing of the app icon (blue, as when connected) serves, at
/// its largest size.
#[cfg(windows)]
fn window_icon() -> Option<window::Icon> {
    let image = tray_icon::Look::Connected
        .images(theme::Accent::default())
        .last()?;
    let rgba = image
        .argb
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[a, r, g, b]| [r, g, b, a])
        .collect();
    window::icon::from_rgba(rgba, image.width, image.height).ok()
}

/// The size the window opens at: wide, with room for the tree and chat.
const WINDOW: Size = Size::new(900.0, 600.0);
/// While anything animates, redraw this often (about 30 fps).
const FRAME: Duration = Duration::from_millis(33);

struct Blurt {
    window: Option<window::Id>,
    /// `None` if the platform has no config directory: nothing is saved.
    dir: Option<PathBuf>,
    config: Config,
    screen: Screen,
    page: Page,
    /// The server form, open over the servers page.
    editor: Option<Editor>,
    /// Shown above whatever page is up, until dismissed.
    error: Option<String>,
    width: f32,
    layout: Layout,
    /// When the layout last switched, to fade the new one in.
    reflowed: Option<Instant>,
    /// Whether the GPU renderer was asked for at startup; the glow can only
    /// show then (and if the GPU started).
    rich: bool,
    hotkeys: Hotkeys,
    /// Counts the times global shortcuts were turned on, so each time
    /// starts a fresh listener.
    hotkeys_run: u32,
    tray: Tray,
    call: Call,
}

/// The call as the system sees it (on Windows 11, the taskbar's microphone
/// button), if it has a place for it.
#[derive(Debug, Default)]
struct Call {
    controller: Option<call::Controller>,
    /// What it was last told, so it's only told again on a change.
    told: Option<call::State>,
}

/// The tray icon, if the desktop has a tray.
#[derive(Debug, Default)]
struct Tray {
    controller: Option<tray::Controller>,
    /// It's in the tray: closing the window can leave blurt running there.
    shown: bool,
    /// What it shows now, so it's only updated on a change.
    showing: Option<TrayShows>,
}

#[derive(Debug, Clone, PartialEq)]
struct TrayShows {
    look: tray_icon::Look,
    accent: Accent,
    status: String,
    window_open: bool,
}

/// The app's ID, as its desktop file is named: what the desktop knows it by
/// (e.g. for its global shortcuts).
const APP_ID: &str = "io.github.haydenmc.blurt";

/// Global hotkeys, as the desktop has them.
#[derive(Debug, Default)]
pub struct Hotkeys {
    controller: Option<hotkeys::Controller>,
    pub bindings: Vec<hotkeys::Binding>,
    /// The desktop can show settings for them.
    pub configurable: bool,
    /// Why there are none, if there are none.
    pub unavailable: Option<String>,
}

enum Screen {
    Idle,
    Connecting {
        server: Server,
        _task: iced::task::Handle,
    },
    Trust {
        server: Server,
        fingerprint: Fingerprint,
        reason: String,
    },
    Connected(Box<Connected>),
}

/// A live connection: its handles, and the room as drawn.
struct Connected {
    session: Session,
    room: Room,
    /// Push-to-talk held with the pointer, and with the space bar.
    pointer_talk: bool,
    key_talk: bool,
    _task: iced::task::Handle,
}

#[derive(Debug, Clone)]
pub enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    Resized(Size),
    Drag,
    Resize(window::Direction),
    Minimize,
    ToggleMaximize,
    CloseWindow,
    DismissError,
    Navigate(Page),

    AddServer,
    EditServer(usize),
    DeleteServer(usize),
    Edit(Field, String),
    SaveServer,
    CancelEdit,

    Connect(usize),
    Cancel,
    Session(session::Event),
    Trust,

    ToggleMute,
    ToggleDeafen,
    PushToTalk(bool),
    SpaceBar(bool),
    /// Tab (true) or Shift+Tab (false): the next or previous text field.
    Tab(bool),
    Volume(f32),
    SaveVolume,
    ToggleVolume,
    Disconnect,
    Join(u32),
    Draft(String),
    SendChat,
    Pivot(Pivot),
    /// Poll the engine: are you transmitting, and how loud.
    Tick,
    /// Redraw an animation.
    Frame,

    SetTheme(Mode),
    SetAccent(Accent),
    SetReduceMotion(bool),
    SetRichVisuals(rich::RichVisuals),
    SetNotificationSounds(bool),
    Hotkey(hotkeys::Event),
    Tray(tray::Event),
    Call(call::Event),
    SetGlobalShortcuts(bool),
    ConfigureHotkeys,
}

impl Blurt {
    fn new() -> (Self, Task<Message>) {
        let (window, open) = open_window();
        let dir = config::dir();
        let (config, error) = match dir.as_deref().map(config::load) {
            Some(Ok(config)) => (config, None),
            Some(Err(e)) => (
                Config::default(),
                Some(format!("could not read the config: {e}")),
            ),
            None => (Config::default(), None),
        };
        let app = Self {
            window: Some(window),
            dir,
            config,
            screen: Screen::Idle,
            page: Page::Servers,
            editor: None,
            error,
            width: WINDOW.width,
            layout: Layout::for_width(WINDOW.width),
            reflowed: None,
            // As `main` chose (or the user's own ICED_BACKEND).
            rich: std::env::var("ICED_BACKEND").map_or(true, |b| !b.starts_with("tiny-skia")),
            hotkeys: Hotkeys::default(),
            hotkeys_run: 0,
            tray: Tray::default(),
            call: Call::default(),
        };
        (app, open)
    }

    fn title(&self, _window: window::Id) -> String {
        match &self.screen {
            Screen::Connected(c) => format!("{} - blurt", c.room.server.label),
            _ => "blurt".to_owned(),
        }
    }

    fn tokens(&self) -> Tokens {
        Tokens::new(self.config.appearance.theme, self.config.appearance.accent)
    }

    fn theme(&self, _window: window::Id) -> iced::Theme {
        self.tokens().iced_theme()
    }

    fn reduce_motion(&self) -> bool {
        self.config.appearance.reduce_motion
    }

    fn save(&mut self) {
        if let Some(dir) = &self.dir
            && let Err(e) = config::save(dir, &self.config)
        {
            self.error = Some(format!("could not save the config: {e}"));
        }
    }

    fn connected(&mut self) -> Option<&mut Connected> {
        match &mut self.screen {
            Screen::Connected(c) => Some(c),
            _ => None,
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.step(message);
        self.show_in_tray();
        self.show_in_call();
        task
    }

    /// Opens the window again, after it was closed to the tray.
    fn open_window(&mut self) -> Task<Message> {
        let (id, open) = open_window();
        self.window = Some(id);
        open
    }

    /// Does what the user (or something else) asked for: a global shortcut
    /// or a tray menu item.
    fn act(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::ToggleMute => self.step(Message::ToggleMute),
            Action::ToggleDeafen => self.step(Message::ToggleDeafen),
            Action::ShowWindow => match self.window {
                Some(id) => window::close(id),
                None => self.open_window(),
            },
            Action::Quit => self.quit(),
            Action::PushToTalk => Task::none(),
        }
    }

    fn quit(&mut self) -> Task<Message> {
        if let Screen::Connected(c) = &self.screen {
            c.session.client.disconnect();
        }
        iced::exit()
    }

    /// Updates the tray icon, if what it shows has changed.
    fn show_in_tray(&mut self) {
        let Some(controller) = &self.tray.controller else {
            return;
        };
        let (connected, transmitting, muted, deafened, status) = match &self.screen {
            Screen::Connected(c) => {
                let channel = c
                    .room
                    .state
                    .current_user()
                    .and_then(|me| c.room.state.channels.get(&me.channel_id))
                    .map(|channel| channel.name.as_str());
                let mut status = c.room.server.label.to_lowercase();
                if let Some(channel) = channel {
                    status = format!("{status} · {}", channel.to_lowercase());
                }
                if c.room.deafened {
                    status += " · deafened";
                } else if c.room.muted {
                    status += " · muted";
                }
                (true, c.room.live(), c.room.muted, c.room.deafened, status)
            }
            Screen::Connecting { server, .. } => (
                false,
                false,
                false,
                false,
                format!("connecting to {}", server.label.to_lowercase()),
            ),
            _ => (false, false, false, false, "not connected".to_owned()),
        };
        let shows = TrayShows {
            look: tray_icon::Look::new(connected, transmitting, muted, deafened),
            accent: self.config.appearance.accent,
            status,
            window_open: self.window.is_some(),
        };
        if self.tray.showing.as_ref() == Some(&shows) {
            return;
        }
        controller.set(tray::State {
            icon: shows.look.images(shows.accent).to_vec(),
            status: shows.status.clone(),
            connected,
            muted,
            deafened,
            window_open: shows.window_open,
        });
        self.tray.showing = Some(shows);
    }

    /// Tells the system about the call, if that has changed (or it asked).
    fn show_in_call(&mut self) {
        let Some(controller) = &self.call.controller else {
            return;
        };
        let state = match &self.screen {
            Screen::Connected(c) => call::State {
                call: Some(c.room.server.label.clone()),
                // Deafening mutes too.
                muted: c.room.muted || c.room.deafened,
            },
            _ => call::State::default(),
        };
        if self.call.told.as_ref() == Some(&state) {
            return;
        }
        controller.set(state.clone());
        self.call.told = Some(state);
    }

    fn step(&mut self, message: Message) -> Task<Message> {
        let now = Instant::now();
        let window = self.window;
        let on_window = |task: fn(window::Id) -> Task<Message>| window.map_or(Task::none(), task);
        match message {
            Message::WindowOpened(id) => {
                log::info!("Window open");
                self.window = Some(id);
                // Opened again from the tray: at the size it opens at.
                self.width = WINDOW.width;
                self.layout = Layout::for_width(WINDOW.width);
                // iced shows a new window only once it's built, which on
                // Windows doesn't bring it forward (opened from the tray,
                // it stayed behind): ask for that once it's up.
                #[cfg(windows)]
                return window::gain_focus(id);
            }
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                // With a tray, blurt carries on there, without a window (and
                // the memory it takes). Without one, closing quits.
                if !self.tray.shown {
                    return self.quit();
                }
            }
            Message::Resized(size) => {
                self.width = size.width;
                let layout = self.layout.resized(size.width);
                if layout != self.layout {
                    self.layout = layout;
                    self.reflowed = Some(now);
                }
            }
            Message::Drag => return on_window(window::drag),
            Message::Resize(direction) => {
                if let Some(id) = window {
                    return window::drag_resize(id, direction);
                }
            }
            Message::Minimize => {
                if let Some(id) = window {
                    return window::minimize(id, true);
                }
            }
            Message::ToggleMaximize => return on_window(window::toggle_maximize),
            Message::CloseWindow => return on_window(window::close),
            Message::DismissError => self.error = None,
            Message::Navigate(page) => {
                if page != Page::Voice || matches!(self.screen, Screen::Connected(_)) {
                    self.page = page;
                }
            }

            Message::AddServer => self.editor = Some(Editor::add()),
            Message::EditServer(index) => {
                if let Some(server) = self.config.servers.get(index) {
                    self.editor = Some(Editor::edit(index, server));
                }
            }
            Message::DeleteServer(index) => {
                if index < self.config.servers.len() {
                    let server = self.config.servers.remove(index);
                    self.config.trusted.remove(&server.address());
                    self.save();
                }
            }
            Message::Edit(field, value) => {
                if let Some(editor) = &mut self.editor {
                    editor.set(field, value);
                }
            }
            Message::SaveServer => {
                if let Some(editor) = &self.editor {
                    match editor.server() {
                        Ok(server) => {
                            match editor.index {
                                Some(i) if i < self.config.servers.len() => {
                                    self.config.servers[i] = server
                                }
                                _ => self.config.servers.push(server),
                            }
                            self.editor = None;
                            self.save();
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            Message::CancelEdit => self.editor = None,

            Message::Connect(index) => {
                if let Some(server) = self.config.servers.get(index).cloned() {
                    if let Screen::Connected(c) = &self.screen {
                        c.session.client.disconnect();
                    }
                    return self.connect(server);
                }
            }
            Message::Cancel => self.screen = Screen::Idle,
            Message::Trust => {
                if let Screen::Trust {
                    server,
                    fingerprint,
                    ..
                } = &self.screen
                {
                    let server = server.clone();
                    self.config.trust(&server, fingerprint);
                    self.save();
                    return self.connect(server);
                }
            }
            Message::Session(event) => self.on_session(event, now),

            Message::ToggleMute => {
                let sounds = self.sounds();
                if let Some(c) = self.connected() {
                    let sound = if c.room.deafened {
                        // Unmuting while deafened undeafens, as in the
                        // official client.
                        c.room.deafened = false;
                        c.room.muted = false;
                        c.session.set_deafened(false, false);
                        Sound::Undeafen
                    } else {
                        c.room.muted = !c.room.muted;
                        c.session.set_muted(c.room.muted);
                        if c.room.muted {
                            Sound::Mute
                        } else {
                            Sound::Unmute
                        }
                    };
                    c.play(sounds, sound);
                }
            }
            Message::ToggleDeafen => {
                let sounds = self.sounds();
                if let Some(c) = self.connected() {
                    c.room.deafened = !c.room.deafened;
                    c.session.set_deafened(c.room.deafened, c.room.muted);
                    let sound = if c.room.deafened {
                        Sound::Deafen
                    } else {
                        Sound::Undeafen
                    };
                    c.play(sounds, sound);
                }
            }
            Message::PushToTalk(held) => {
                if let Some(c) = self.connected() {
                    c.pointer_talk = held;
                    c.apply_push_to_talk();
                }
            }
            Message::SpaceBar(held) => {
                let on_voice = self.page == Page::Voice;
                if let Some(c) = self.connected() {
                    // Only on the voice page, so space still works in forms.
                    c.key_talk = held && on_voice;
                    c.apply_push_to_talk();
                }
            }
            Message::Tab(forward) => {
                return if forward {
                    widget::operation::focus_next()
                } else {
                    widget::operation::focus_previous()
                };
            }
            Message::Volume(volume) => {
                self.config.audio.output_volume = volume;
                if let Some(c) = self.connected() {
                    c.session.engine.set_output_volume(volume);
                }
            }
            Message::SaveVolume => self.save(),
            Message::ToggleVolume => {
                if let Some(c) = self.connected() {
                    c.room.volume_open = !c.room.volume_open;
                }
            }
            Message::Disconnect => {
                if let Screen::Connected(c) = &self.screen {
                    c.farewell(self.sounds());
                    c.session.client.disconnect();
                }
                self.screen = Screen::Idle;
                self.page = Page::Servers;
            }
            Message::Join(channel) => {
                if let Screen::Connected(c) = &self.screen
                    && let Err(e) = c.session.client.set_channel(channel)
                {
                    self.error = Some(format!("could not join: {e}"));
                }
            }
            Message::Draft(draft) => {
                if let Some(c) = self.connected() {
                    c.room.draft = draft;
                }
            }
            Message::SendChat => {
                if let Some(c) = self.connected() {
                    let draft = std::mem::take(&mut c.room.draft);
                    if !draft.trim().is_empty() {
                        match c
                            .session
                            .client
                            .send_channel_message(html::escape(&draft), None)
                        {
                            Ok(()) => {
                                c.refresh(now);
                            }
                            Err(e) => {
                                c.room.draft = draft;
                                self.error = Some(format!("could not send: {e}"));
                            }
                        }
                    }
                }
            }
            Message::Pivot(pivot) => {
                if let Some(c) = self.connected()
                    && c.room.pivot != pivot
                {
                    c.room.pivot = pivot;
                    c.room.pivot_changed = Some(now);
                }
            }
            Message::Tick => {
                if let Some(c) = self.connected() {
                    let stats = c.session.engine.stats();
                    c.room.transmitting = stats.transmitting;
                    c.room.mic_level = stats.mic_level;
                    c.room.observe_talking(now);
                }
                // Between frames, notice when the glow needs to start.
                if self.glow()
                    && let Some(c) = self.connected()
                {
                    c.room.levels = c.session.engine.speaker_levels();
                    c.room.spectrum = c.session.engine.spectrum();
                    c.room.step_glow(now);
                }
            }
            Message::Frame => {
                let glow = self.glow();
                if let Some(c) = self.connected() {
                    c.room.mic_level = c.session.engine.stats().mic_level;
                    if glow {
                        c.room.levels = c.session.engine.speaker_levels();
                        c.room.spectrum = c.session.engine.spectrum();
                        c.room.step_glow(now);
                    }
                }
            }

            Message::SetTheme(mode) => {
                self.config.appearance.theme = mode;
                self.save();
            }
            Message::SetAccent(accent) => {
                self.config.appearance.accent = accent;
                self.save();
            }
            Message::Hotkey(event) => match event {
                hotkeys::Event::Ready(controller) => self.hotkeys.controller = Some(controller),
                hotkeys::Event::Bound {
                    bindings,
                    configurable,
                } => {
                    self.hotkeys.bindings = bindings;
                    self.hotkeys.configurable = configurable;
                    self.hotkeys.unavailable = None;
                }
                hotkeys::Event::Pressed(action) => return self.act(action),
                hotkeys::Event::Released(_) => {}
                hotkeys::Event::Unavailable(reason) => {
                    self.hotkeys = Hotkeys {
                        unavailable: Some(reason),
                        ..Hotkeys::default()
                    };
                }
                hotkeys::Event::Stopped => self.hotkeys = Hotkeys::default(),
            },
            Message::Call(event) => match event {
                call::Event::Ready(controller) => {
                    self.call = Call {
                        controller: Some(controller),
                        told: None,
                    };
                }
                call::Event::MuteRequested(mute) => {
                    // The system waits for an answer, even if nothing
                    // changes: tell it again either way.
                    self.call.told = None;
                    if let Some(c) = self.connected()
                        && (c.room.muted || c.room.deafened) != mute
                    {
                        // Unmuting while deafened undeafens, as mute does.
                        return self.act(Action::ToggleMute);
                    }
                }
                call::Event::Unavailable(_) => self.call = Call::default(),
            },
            Message::Tray(event) => match event {
                tray::Event::Ready(controller) => {
                    self.tray.controller = Some(controller);
                    self.tray.showing = None;
                }
                tray::Event::Shown => self.tray.shown = true,
                tray::Event::Action(action) => return self.act(action),
                tray::Event::Unavailable(_) => {
                    self.tray = Tray::default();
                    // Hidden in a tray that's gone: show the window again.
                    if self.window.is_none() {
                        return self.open_window();
                    }
                }
            },
            Message::SetGlobalShortcuts(on) => {
                self.config.shortcuts.global = on;
                self.save();
                if on {
                    // Starting over: forget why it failed last time.
                    self.hotkeys = Hotkeys::default();
                    self.hotkeys_run += 1;
                } else if let Some(controller) = &self.hotkeys.controller {
                    // The listener keeps running until it has let go.
                    controller.stop();
                }
            }
            Message::ConfigureHotkeys => {
                if let Some(controller) = &self.hotkeys.controller {
                    controller.configure();
                }
            }
            Message::SetRichVisuals(rich) => {
                self.config.appearance.rich_visuals = rich;
                self.save();
            }
            Message::SetReduceMotion(reduce) => {
                self.config.appearance.reduce_motion = reduce;
                self.save();
            }
            Message::SetNotificationSounds(on) => {
                self.config.audio.notification_sounds = on;
                self.save();
            }
        }
        Task::none()
    }

    fn connect(&mut self, server: Server) -> Task<Message> {
        self.error = None;
        let identity = match &self.dir {
            Some(dir) => match config::identity(dir, &server.username) {
                Ok(identity) => Some(identity),
                Err(e) => {
                    log::warn!("No client certificate: {e}");
                    None
                }
            },
            None => None,
        };
        let request = session::Request {
            options: ConnectionOptions {
                server: server.host.clone(),
                port: Some(server.port),
                username: Some(server.username.clone()),
                password: server.password.clone(),
                pinned_certificate: self.config.trusted_certificate(&server),
                identity,
                client_release: Some(concat!("blurt ", env!("CARGO_PKG_VERSION")).into()),
                ..Default::default()
            },
            engine: EngineConfig::default(),
        };
        let (task, handle) = Task::run(session::run(request), Message::Session).abortable();
        self.screen = Screen::Connecting {
            server,
            _task: handle.abort_on_drop(),
        };
        task
    }

    fn on_session(&mut self, event: session::Event, now: Instant) {
        use session::Event;
        match event {
            Event::Connected(session) => {
                let screen = std::mem::replace(&mut self.screen, Screen::Idle);
                match screen {
                    Screen::Connecting { server, _task } => {
                        session
                            .engine
                            .set_output_volume(self.config.audio.output_volume);
                        let mut c = Connected {
                            session,
                            room: Room::new(server),
                            pointer_talk: false,
                            key_talk: false,
                            _task,
                        };
                        c.refresh(now);
                        c.play(self.sounds(), Sound::Join);
                        self.screen = Screen::Connected(Box::new(c));
                        self.page = Page::Voice;
                        self.editor = None;
                    }
                    // Cancelled meanwhile.
                    other => {
                        session.client.disconnect();
                        self.screen = other;
                    }
                }
            }
            Event::Untrusted {
                fingerprint,
                reason,
            } => {
                if let Screen::Connecting { server, .. } = &self.screen {
                    self.screen = Screen::Trust {
                        server: server.clone(),
                        fingerprint,
                        reason,
                    };
                }
            }
            Event::Changed => {
                let sounds = self.sounds();
                if let Some(c) = self.connected() {
                    let news = c.refresh(now);
                    // One sound a change, the most interesting.
                    let sound = if news.message {
                        Some(Sound::Message)
                    } else if news.joined {
                        Some(Sound::Join)
                    } else if news.left {
                        Some(Sound::Leave)
                    } else {
                        None
                    };
                    if let Some(sound) = sound {
                        c.play(sounds, sound);
                    }
                }
            }
            Event::Ended(error) => {
                if let Screen::Connected(c) = &self.screen {
                    c.farewell(self.sounds());
                }
                if matches!(
                    self.screen,
                    Screen::Connected(_) | Screen::Connecting { .. }
                ) {
                    self.screen = Screen::Idle;
                    if self.page == Page::Voice {
                        self.page = Page::Servers;
                    }
                    if error.is_some() {
                        self.error = error.map(|e| e.to_lowercase());
                    }
                }
            }
        }
    }

    /// Whether notification sounds are on.
    fn sounds(&self) -> bool {
        self.config.audio.notification_sounds
    }

    /// Whether the room glow can show: rich visuals asked for at startup,
    /// the GPU running (as far as is known yet), and motion allowed.
    fn glow(&self) -> bool {
        cfg!(feature = "gpu")
            && self.rich
            && rich::renderer() != rich::Renderer::Cpu
            && !self.reduce_motion()
    }

    /// Whether anything on screen is moving, so frames are worth drawing.
    fn animating(&self, now: Instant) -> bool {
        let running =
            |at: Option<Instant>, duration| at.is_some_and(|at| now.duration_since(at) < duration);
        // Nothing to draw on with the window closed.
        if self.reduce_motion() || self.window.is_none() {
            return false;
        }
        if running(self.reflowed, theme::motion::REFLOW) {
            return true;
        }
        match &self.screen {
            Screen::Connected(c) => {
                running(c.room.pivot_changed, theme::motion::PIVOT)
                    || (self.glow()
                        && self.page == Page::Voice
                        && (c.room.sounding() || c.room.glow.visible()))
                    || c.room.talking.values().any(|a| a.is_animating(now))
                    // Your meter moves while you talk.
                    || (self.page == Page::Voice && c.room.live())
            }
            _ => false,
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let now = Instant::now();
        let shown = self.reflowed.map_or(1.0, |at| {
            progress(at, now, theme::motion::REFLOW, self.reduce_motion())
        });
        let t = self.tokens().faded(shown);
        let connected = match &self.screen {
            Screen::Connected(c) => Some(c.as_ref()),
            _ => None,
        };

        let body: Element<'_, Message> = match (&self.screen, self.page) {
            (Screen::Connected(c), Page::Voice) => voice::view(&voice::Voice {
                t,
                room: &c.room,
                now,
                width: self.width,
                layout: self.layout,
                reduce_motion: self.reduce_motion(),
                volume: self.config.audio.output_volume,
                glow: self.glow(),
            }),
            (_, Page::Settings) => pages::settings(t, &self.config, self.rich, &self.hotkeys),
            (Screen::Connecting { server, .. }, _) => pages::connecting(t, server),
            (
                Screen::Trust {
                    server,
                    fingerprint,
                    reason,
                },
                _,
            ) => pages::trust(t, server, fingerprint, reason),
            (_, _) => match &self.editor {
                Some(editor) => pages::editor(t, editor),
                None => pages::servers(t, &self.config, connected.map(|c| &c.room.server)),
            },
        };

        // The title bar carries the page pivots (or, on the compact voice
        // screen, the server's name), so there's no separate nav row.
        let server = connected.map(|c| c.room.server.label.as_str());
        let header = match server {
            Some(server) if self.page == Page::Voice && self.layout == Layout::Compact => {
                voice::compact_header(t, server)
            }
            _ => voice::nav_row(t, self.page, connected.is_some(), server),
        };
        let mut page = column![chrome::title_bar(t, header)];
        if let Some(error) = &self.error {
            page = page.push(pages::error_banner(t, error));
        }
        let page = container(page.push(body))
            .width(Fill)
            .height(Fill)
            .style(theme::fill(t.bg));
        chrome::with_resize_edges(page.into())
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::close_events().map(Message::WindowClosed),
            Subscription::run_with(APP_ID, |id| tray::listen(id, tray::State::default()))
                .map(Message::Tray),
            Subscription::run(call::listen).map(Message::Call),
            window::resize_events().map(|(_, size)| Message::Resized(size)),
            // Text fields let Tab through; it moves between them.
            keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Tab),
                    modifiers,
                    ..
                } => Some(Message::Tab(!modifiers.shift())),
                _ => None,
            }),
        ];
        // Global shortcuts, once asked for, until they're let go.
        if self.config.shortcuts.global || self.hotkeys.controller.is_some() {
            subscriptions.push(
                Subscription::run_with((APP_ID, self.hotkeys_run), |(id, _)| hotkeys::listen(id))
                    .map(Message::Hotkey),
            );
        }
        if let Screen::Connected(_) = self.screen {
            // Transmit state, your level and the talking line's times; only
            // while connected.
            subscriptions
                .push(iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick));
            subscriptions.push(keyboard::listen().filter_map(|event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Space),
                    repeat: false,
                    ..
                } => Some(Message::SpaceBar(true)),
                keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(keyboard::key::Named::Space),
                    ..
                } => Some(Message::SpaceBar(false)),
                _ => None,
            }));
        }
        if self.animating(Instant::now()) {
            subscriptions.push(iced::time::every(FRAME).map(|_| Message::Frame));
        }
        Subscription::batch(subscriptions)
    }
}

impl Connected {
    /// Re-reads the client's state after it changed; returns what's new
    /// in the transcript.
    fn refresh(&mut self, now: Instant) -> transcript::News {
        let state = self.session.client.state().lock().unwrap().clone();
        self.room.update(state, now)
    }

    fn play(&self, enabled: bool, sound: Sound) {
        if enabled {
            sound.play(&self.session.engine);
        }
    }

    /// Before the connection ends: the leave sound, if wanted, played out
    /// by the engine after the session is gone (it finishes its cues, then
    /// stops), with the mic shut meanwhile.
    fn farewell(&self, sounds: bool) {
        if !sounds {
            return;
        }
        let engine = Arc::clone(&self.session.engine);
        engine.set_muted(true);
        Sound::Leave.play(&engine);
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(4);
            while !engine.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(50));
            }
            drop(engine);
        });
    }

    fn apply_push_to_talk(&self) {
        self.session
            .engine
            .set_push_to_talk(self.pointer_talk || self.key_talk);
    }
}

/// Channels depth-first from the root, siblings by name, with their depth.
fn channel_order(state: &MumbleState) -> Vec<(usize, &mumble_protocol::state::Channel)> {
    fn visit<'a>(
        state: &'a MumbleState,
        parent: Option<u32>,
        depth: usize,
        out: &mut Vec<(usize, &'a mumble_protocol::state::Channel)>,
    ) {
        let mut children: Vec<_> = state
            .channels
            .values()
            .filter(|c| c.parent == parent && Some(c.channel_id) != parent)
            .collect();
        children.sort_by_key(|c| c.name.to_lowercase());
        for child in children {
            out.push((depth, child));
            // The root has no parent; guard against a server saying a
            // channel is its own ancestor.
            if depth < 32 {
                visit(state, Some(child.channel_id), depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(state, None, 0, &mut out);
    out
}
