//! blurt: a small, fast, native Mumble client.

mod config;
mod html;
mod session;

use std::path::PathBuf;
use std::time::Duration;

use blurt_audio::EngineConfig;
use config::{Config, Server};
use iced::widget::{button, center, column, container, row, scrollable, space, text, text_input};
use iced::{Center, Element, Fill, FillPortion, Font, Subscription, Task, window};
use mumble_protocol::client::{ConnectionOptions, Fingerprint};
use mumble_protocol::state::{MumbleState, TextMessage};
use session::Session;

fn main() -> iced::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    // A daemon outlives its windows, which is how close-to-tray will work.
    // Until the tray exists (milestone 5), closing the last window quits.
    iced::daemon(Blurt::new, Blurt::update, Blurt::view)
        .title(Blurt::title)
        .subscription(Blurt::subscription)
        .run()
}

struct Blurt {
    window: Option<window::Id>,
    /// `None` if the platform has no config directory: nothing is saved.
    dir: Option<PathBuf>,
    config: Config,
    screen: Screen,
    /// Shown above whatever screen is up, until dismissed.
    error: Option<String>,
}

enum Screen {
    Servers(Option<Editor>),
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

/// The add/edit server form.
#[derive(Default)]
struct Editor {
    /// Which saved server this edits; `None` adds one.
    index: Option<usize>,
    label: String,
    host: String,
    port: String,
    username: String,
    password: String,
}

struct Connected {
    server: Server,
    session: Session,
    /// The client's state as of the last change, for drawing.
    state: MumbleState,
    muted: bool,
    deafened: bool,
    transmitting: bool,
    draft: String,
    _task: iced::task::Handle,
}

#[derive(Debug, Clone)]
enum Field {
    Label,
    Host,
    Port,
    Username,
    Password,
}

#[derive(Debug, Clone)]
enum Message {
    WindowOpened(window::Id),
    WindowClosed(window::Id),
    DismissError,

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
    Disconnect,
    Join(u32),
    Draft(String),
    SendChat,
    Tick,
}

impl Blurt {
    fn new() -> (Self, Task<Message>) {
        let (_id, open) = window::open(window::Settings {
            size: iced::Size::new(900.0, 600.0),
            min_size: Some(iced::Size::new(360.0, 240.0)),
            ..window::Settings::default()
        });
        let dir = config::dir();
        let (config, error) = match dir.as_deref().map(config::load) {
            Some(Ok(config)) => (config, None),
            Some(Err(e)) => (
                Config::default(),
                Some(format!("Could not read config: {e}")),
            ),
            None => (Config::default(), None),
        };
        let app = Self {
            window: None,
            dir,
            config,
            screen: Screen::Servers(None),
            error,
        };
        (app, open.map(Message::WindowOpened))
    }

    fn title(&self, _window: window::Id) -> String {
        match &self.screen {
            Screen::Connected(c) => format!("{} - blurt", c.server.label),
            _ => "blurt".to_owned(),
        }
    }

    fn save(&mut self) {
        if let Some(dir) = &self.dir
            && let Err(e) = config::save(dir, &self.config)
        {
            self.error = Some(format!("Could not save config: {e}"));
        }
    }

    fn update(&mut self, message: Message) -> Task<Message> {
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
            Message::DismissError => self.error = None,

            Message::AddServer => {
                self.screen = Screen::Servers(Some(Editor {
                    port: config::DEFAULT_PORT.to_string(),
                    ..Default::default()
                }))
            }
            Message::EditServer(index) => {
                if let Some(server) = self.config.servers.get(index) {
                    self.screen = Screen::Servers(Some(Editor {
                        index: Some(index),
                        label: server.label.clone(),
                        host: server.host.clone(),
                        port: server.port.to_string(),
                        username: server.username.clone(),
                        password: server.password.clone().unwrap_or_default(),
                    }));
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
                if let Screen::Servers(Some(editor)) = &mut self.screen {
                    *match field {
                        Field::Label => &mut editor.label,
                        Field::Host => &mut editor.host,
                        Field::Port => &mut editor.port,
                        Field::Username => &mut editor.username,
                        Field::Password => &mut editor.password,
                    } = value;
                }
            }
            Message::SaveServer => {
                if let Screen::Servers(Some(editor)) = &self.screen {
                    match editor.server() {
                        Ok(server) => {
                            match editor.index {
                                Some(i) if i < self.config.servers.len() => {
                                    self.config.servers[i] = server
                                }
                                _ => self.config.servers.push(server),
                            }
                            self.save();
                            self.screen = Screen::Servers(None);
                        }
                        Err(e) => self.error = Some(e),
                    }
                }
            }
            Message::CancelEdit => self.screen = Screen::Servers(None),

            Message::Connect(index) => {
                if let Some(server) = self.config.servers.get(index).cloned() {
                    return self.connect(server);
                }
            }
            Message::Cancel => self.screen = Screen::Servers(None),
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
            Message::Session(event) => self.on_session(event),

            Message::ToggleMute => {
                if let Screen::Connected(c) = &mut self.screen {
                    if c.deafened {
                        // Unmuting while deafened undeafens, as in the
                        // official client.
                        c.deafened = false;
                        c.muted = false;
                        c.session.set_deafened(false, false);
                    } else {
                        c.muted = !c.muted;
                        c.session.set_muted(c.muted);
                    }
                }
            }
            Message::ToggleDeafen => {
                if let Screen::Connected(c) = &mut self.screen {
                    c.deafened = !c.deafened;
                    c.session.set_deafened(c.deafened, c.muted);
                }
            }
            Message::Disconnect => {
                if let Screen::Connected(c) = &self.screen {
                    c.session.client.disconnect();
                }
                self.screen = Screen::Servers(None);
            }
            Message::Join(channel) => {
                if let Screen::Connected(c) = &self.screen
                    && let Err(e) = c.session.client.set_channel(channel)
                {
                    self.error = Some(format!("Could not join: {e}"));
                }
            }
            Message::Draft(draft) => {
                if let Screen::Connected(c) = &mut self.screen {
                    c.draft = draft;
                }
            }
            Message::SendChat => {
                if let Screen::Connected(c) = &mut self.screen {
                    let draft = std::mem::take(&mut c.draft);
                    if !draft.trim().is_empty() {
                        match c
                            .session
                            .client
                            .send_channel_message(html::escape(&draft), None)
                        {
                            Ok(()) => c.state = c.session.client.state().lock().unwrap().clone(),
                            Err(e) => {
                                c.draft = draft;
                                self.error = Some(format!("Could not send: {e}"));
                            }
                        }
                    }
                }
            }
            Message::Tick => {
                if let Screen::Connected(c) = &mut self.screen {
                    c.transmitting = c.session.engine.stats().transmitting;
                }
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

    fn on_session(&mut self, event: session::Event) {
        use session::Event;
        match event {
            Event::Connected(session) => {
                let screen = std::mem::replace(&mut self.screen, Screen::Servers(None));
                match screen {
                    Screen::Connecting { server, _task } => {
                        let state = session.client.state().lock().unwrap().clone();
                        self.screen = Screen::Connected(Box::new(Connected {
                            server,
                            session,
                            state,
                            muted: false,
                            deafened: false,
                            transmitting: false,
                            draft: String::new(),
                            _task,
                        }));
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
                if let Screen::Connected(c) = &mut self.screen {
                    c.state = c.session.client.state().lock().unwrap().clone();
                }
            }
            Event::Ended(error) => {
                if matches!(
                    self.screen,
                    Screen::Connected(_) | Screen::Connecting { .. }
                ) {
                    self.screen = Screen::Servers(None);
                    if error.is_some() {
                        self.error = error;
                    }
                }
            }
        }
    }

    fn view(&self, _window: window::Id) -> Element<'_, Message> {
        let screen = match &self.screen {
            Screen::Servers(editor) => self.servers(editor.as_ref()),
            Screen::Connecting { server, .. } => center(
                column![
                    text(format!("Connecting to {}...", server.label)),
                    button("Cancel").on_press(Message::Cancel),
                ]
                .spacing(12)
                .align_x(Center),
            )
            .into(),
            Screen::Trust {
                server,
                fingerprint,
                reason,
            } => trust(server, fingerprint, reason),
            Screen::Connected(c) => connected(c),
        };
        match &self.error {
            Some(error) => column![
                container(
                    row![
                        text(error).width(Fill),
                        button("Dismiss")
                            .on_press(Message::DismissError)
                            .style(button::text),
                    ]
                    .align_y(Center)
                )
                .padding([4, 12])
                .style(container::danger),
                screen,
            ]
            .into(),
            None => screen,
        }
    }

    fn servers<'a>(&'a self, editor: Option<&'a Editor>) -> Element<'a, Message> {
        if let Some(editor) = editor {
            return editor.view();
        }
        let mut list = column![].spacing(8);
        if self.config.servers.is_empty() {
            list = list.push(text("No servers yet."));
        }
        for (i, server) in self.config.servers.iter().enumerate() {
            list = list.push(
                container(
                    row![
                        column![
                            text(&server.label).font(Font {
                                weight: iced::font::Weight::Bold,
                                ..Font::DEFAULT
                            }),
                            text(format!("{}@{}", server.username, server.address())).size(13),
                        ]
                        .width(Fill),
                        button("Connect").on_press(Message::Connect(i)),
                        button("Edit")
                            .on_press(Message::EditServer(i))
                            .style(button::secondary),
                        button("Delete")
                            .on_press(Message::DeleteServer(i))
                            .style(button::danger),
                    ]
                    .spacing(8)
                    .align_y(Center),
                )
                .padding(12)
                .style(container::rounded_box),
            );
        }
        let page = column![
            row![
                text("Servers").size(24).width(Fill),
                button("Add server").on_press(Message::AddServer),
            ]
            .align_y(Center),
            scrollable(list),
        ]
        .spacing(16)
        .max_width(640);
        container(page).padding(24).center_x(Fill).into()
    }

    fn subscription(&self) -> Subscription<Message> {
        let closed = window::close_events().map(Message::WindowClosed);
        match self.screen {
            // For the talking indicator; only while connected.
            Screen::Connected(_) => Subscription::batch([
                closed,
                iced::time::every(Duration::from_millis(100)).map(|_| Message::Tick),
            ]),
            _ => closed,
        }
    }
}

impl Editor {
    fn server(&self) -> Result<Server, String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("The server needs an address".into());
        }
        let username = self.username.trim();
        if username.is_empty() {
            return Err("Pick a username".into());
        }
        let port = match self.port.trim() {
            "" => config::DEFAULT_PORT,
            port => port.parse().map_err(|_| format!("{port:?} isn't a port"))?,
        };
        let label = match self.label.trim() {
            "" => host,
            label => label,
        };
        Ok(Server {
            label: label.to_owned(),
            host: host.to_owned(),
            port,
            username: username.to_owned(),
            password: (!self.password.is_empty()).then(|| self.password.clone()),
        })
    }

    fn view(&self) -> Element<'_, Message> {
        let field = |label: &'static str, value: &str, field: Field| {
            column![
                text(label).size(13),
                text_input("", value)
                    .on_input(move |v| Message::Edit(field.clone(), v))
                    .on_submit(Message::SaveServer),
            ]
            .spacing(4)
        };
        let form = column![
            text(if self.index.is_some() {
                "Edit server"
            } else {
                "Add server"
            })
            .size(24),
            field("Address", &self.host, Field::Host),
            field("Port", &self.port, Field::Port),
            field("Username", &self.username, Field::Username),
            column![
                text("Password (if the server has one)").size(13),
                text_input("", &self.password)
                    .secure(true)
                    .on_input(|v| Message::Edit(Field::Password, v))
                    .on_submit(Message::SaveServer),
            ]
            .spacing(4),
            field("Name (optional)", &self.label, Field::Label),
            row![
                button("Save").on_press(Message::SaveServer),
                button("Cancel")
                    .on_press(Message::CancelEdit)
                    .style(button::secondary),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .max_width(480);
        container(form).padding(24).center_x(Fill).into()
    }
}

fn trust<'a>(
    server: &'a Server,
    fingerprint: &Fingerprint,
    reason: &'a str,
) -> Element<'a, Message> {
    center(
        column![
            text(format!(
                "{} isn't using a certificate blurt can verify.",
                server.label
            ))
            .size(18),
            text(format!("({reason})")).size(13),
            text("Its certificate's SHA-256 fingerprint is:"),
            text(fingerprint.to_string()).font(Font::MONOSPACE).size(13),
            text(
                "If you trust this server, blurt will remember the certificate and \
                 warn you if it ever changes."
            ),
            row![
                button("Trust and connect").on_press(Message::Trust),
                button("Cancel")
                    .on_press(Message::Cancel)
                    .style(button::secondary),
            ]
            .spacing(8),
        ]
        .spacing(12)
        .max_width(560),
    )
    .padding(24)
    .into()
}

fn connected(c: &Connected) -> Element<'_, Message> {
    let me = c.state.current_user();
    let toolbar = row![
        text(&c.server.label).size(18),
        text(if c.transmitting { "talking" } else { "" }).size(13),
        space::horizontal(),
        toggle(
            if c.muted || c.deafened {
                "Unmute"
            } else {
                "Mute"
            },
            c.muted || c.deafened,
            Message::ToggleMute
        ),
        toggle(
            if c.deafened { "Undeafen" } else { "Deafen" },
            c.deafened,
            Message::ToggleDeafen
        ),
        button("Disconnect")
            .on_press(Message::Disconnect)
            .style(button::secondary),
    ]
    .spacing(8)
    .align_y(Center);

    let mut tree = column![].spacing(2);
    for (depth, channel) in channel_order(&c.state) {
        let here = me.is_some_and(|me| me.channel_id == channel.channel_id);
        let name = text(&channel.name).font(Font {
            weight: if here {
                iced::font::Weight::Bold
            } else {
                iced::font::Weight::Normal
            },
            ..Font::DEFAULT
        });
        tree = tree.push(row![
            space().width(depth as f32 * 16.0),
            button(name)
                .on_press(Message::Join(channel.channel_id))
                .style(button::text)
                .padding([2, 4]),
        ]);
        let mut users: Vec<_> = c
            .state
            .users
            .values()
            .filter(|u| u.channel_id == channel.channel_id)
            .collect();
        users.sort_by_key(|u| u.name.to_lowercase());
        for user in users {
            let speaking = if user.is_me {
                c.transmitting
            } else {
                user.is_speaking
            };
            let mut flags = Vec::new();
            if user.self_deaf || user.deaf {
                flags.push("deafened");
            } else if user.self_mute || user.mute || user.suppress {
                flags.push("muted");
            }
            let marker = if speaking { "●" } else { "○" };
            tree = tree.push(
                row![
                    space().width(depth as f32 * 16.0 + 12.0),
                    text(marker)
                        .size(12)
                        .style(move |theme: &iced::Theme| text::Style {
                            color: Some(if speaking {
                                theme.extended_palette().success.base.color
                            } else {
                                theme.extended_palette().background.strong.color
                            }),
                        }),
                    text(&user.name).font(Font {
                        weight: if user.is_me {
                            iced::font::Weight::Bold
                        } else {
                            iced::font::Weight::Normal
                        },
                        ..Font::DEFAULT
                    }),
                    text(flags.join(", ")).size(12),
                ]
                .spacing(6)
                .align_y(Center),
            );
        }
    }

    let mut log = column![].spacing(4);
    for message in c.state.chat_messages() {
        log = log.push(match message {
            TextMessage::ChatTextMessage(m) => {
                let from = m
                    .actor
                    .and_then(|s| c.state.users.get(&s))
                    .map_or("server", |u| u.name.as_str());
                row![
                    text(format!("{from}:")).font(Font {
                        weight: iced::font::Weight::Bold,
                        ..Font::DEFAULT
                    }),
                    text(html::to_text(&m.content)),
                ]
                .spacing(6)
            }
            TextMessage::ServerTextMessage(m) => row![text(html::to_text(&m.content)).size(13)],
        });
    }
    let channel = me
        .and_then(|me| c.state.channels.get(&me.channel_id))
        .map_or("", |ch| ch.name.as_str());
    let chat = column![
        scrollable(log).anchor_bottom().height(Fill).width(Fill),
        text_input(&format!("Message {channel}"), &c.draft)
            .on_input(Message::Draft)
            .on_submit(Message::SendChat),
    ]
    .spacing(8);

    column![
        toolbar,
        row![
            container(scrollable(tree).width(Fill))
                .width(FillPortion(2))
                .height(Fill)
                .padding(8)
                .style(container::bordered_box),
            container(chat).width(FillPortion(3)).height(Fill),
        ]
        .spacing(12),
    ]
    .spacing(12)
    .padding(12)
    .into()
}

fn toggle(label: &str, on: bool, message: Message) -> Element<'_, Message> {
    button(text(label))
        .on_press(message)
        .style(if on {
            button::danger
        } else {
            button::secondary
        })
        .into()
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
