//! blurt: a small, fast, native Mumble client.

mod config;
mod html;
mod icons;
mod room;
mod session;
#[cfg(test)]
mod snapshots;
mod talk;
mod theme;
mod transcript;
mod ui;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use blurt_audio::EngineConfig;
use config::{Config, Server};
use iced::widget::{column, container};
use iced::{Element, Fill, Size, Subscription, Task, keyboard, window};
use mumble_protocol::client::{ConnectionOptions, Fingerprint};
use mumble_protocol::state::MumbleState;
use room::Room;
use session::Session;
use theme::{Accent, Mode, Tokens};
use ui::pages::{self, Editor, Field};
use ui::voice::{self, progress};
use ui::{Layout, Page, Pivot, chrome};

fn main() -> iced::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // A daemon outlives its windows, which is how close-to-tray will work.
    // Until the tray exists (milestone 5), closing the last window quits.
    iced::daemon(Blurt::new, Blurt::update, Blurt::view)
        .title(Blurt::title)
        .theme(Blurt::theme)
        .subscription(Blurt::subscription)
        .run()
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
}

impl Blurt {
    fn new() -> (Self, Task<Message>) {
        let (_id, open) = window::open(window::Settings {
            size: WINDOW,
            min_size: Some(Size::new(360.0, 480.0)),
            // blurt draws its own title bar.
            decorations: false,
            ..window::Settings::default()
        });
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
            window: None,
            dir,
            config,
            screen: Screen::Idle,
            page: Page::Servers,
            editor: None,
            error,
            width: WINDOW.width,
            layout: Layout::for_width(WINDOW.width),
            reflowed: None,
        };
        (app, open.map(Message::WindowOpened))
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
        let now = Instant::now();
        let window = self.window;
        let on_window = |task: fn(window::Id) -> Task<Message>| window.map_or(Task::none(), task);
        match message {
            Message::WindowOpened(id) => self.window = Some(id),
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                if let Screen::Connected(c) = &self.screen {
                    c.session.client.disconnect();
                }
                // TODO(milestone 5): stay alive in the tray instead.
                return iced::exit();
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
                if let Some(c) = self.connected() {
                    if c.room.deafened {
                        // Unmuting while deafened undeafens, as in the
                        // official client.
                        c.room.deafened = false;
                        c.room.muted = false;
                        c.session.set_deafened(false, false);
                    } else {
                        c.room.muted = !c.room.muted;
                        c.session.set_muted(c.room.muted);
                    }
                }
            }
            Message::ToggleDeafen => {
                if let Some(c) = self.connected() {
                    c.room.deafened = !c.room.deafened;
                    c.session.set_deafened(c.room.deafened, c.room.muted);
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
                            Ok(()) => c.refresh(now),
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
            }
            Message::Frame => {
                if let Some(c) = self.connected() {
                    c.room.mic_level = c.session.engine.stats().mic_level;
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
            Message::SetReduceMotion(reduce) => {
                self.config.appearance.reduce_motion = reduce;
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
                if let Some(c) = self.connected() {
                    c.refresh(now);
                }
            }
            Event::Ended(error) => {
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

    /// Whether anything on screen is moving, so frames are worth drawing.
    fn animating(&self, now: Instant) -> bool {
        let running =
            |at: Option<Instant>, duration| at.is_some_and(|at| now.duration_since(at) < duration);
        if self.reduce_motion() {
            return false;
        }
        if running(self.reflowed, theme::motion::REFLOW) {
            return true;
        }
        match &self.screen {
            Screen::Connected(c) => {
                running(c.room.pivot_changed, theme::motion::PIVOT)
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
            }),
            (_, Page::Settings) => self.page_with_nav(t, pages::settings(t, &self.config)),
            (Screen::Connecting { server, .. }, _) => {
                self.page_with_nav(t, pages::connecting(t, server))
            }
            (
                Screen::Trust {
                    server,
                    fingerprint,
                    reason,
                },
                _,
            ) => self.page_with_nav(t, pages::trust(t, server, fingerprint, reason)),
            (_, _) => self.page_with_nav(
                t,
                match &self.editor {
                    Some(editor) => pages::editor(t, editor),
                    None => pages::servers(t, &self.config, connected.map(|c| &c.room.server)),
                },
            ),
        };

        let mut page = column![chrome::title_bar(t, None)];
        if let Some(error) = &self.error {
            page = page.push(pages::error_banner(t, error));
        }
        let page = container(page.push(body))
            .width(Fill)
            .height(Fill)
            .style(theme::fill(t.bg));
        chrome::with_resize_edges(page.into())
    }

    fn page_with_nav<'a>(
        &'a self,
        t: Tokens,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let server = match &self.screen {
            Screen::Connected(c) => Some(c.room.server.label.as_str()),
            _ => None,
        };
        column![
            voice::nav_row(
                t,
                self.page,
                matches!(self.screen, Screen::Connected(_)),
                server
            ),
            content,
        ]
        .height(Fill)
        .into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subscriptions = vec![
            window::close_events().map(Message::WindowClosed),
            window::resize_events().map(|(_, size)| Message::Resized(size)),
        ];
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
    /// Re-reads the client's state after it changed.
    fn refresh(&mut self, now: Instant) {
        let state = self.session.client.state().lock().unwrap().clone();
        self.room.update(state, now);
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
