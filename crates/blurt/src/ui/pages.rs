//! The pages around the voice screen: servers (and the server form),
//! connecting, certificate trust, settings, and the error banner.

use iced::font::Weight;
use iced::widget::text::Wrapping;
use iced::widget::{Column, button, column, container, row, scrollable, space, text, text_input};
use iced::{Center, Element, Fill, Font, Padding};
use mumble_protocol::client::Fingerprint;

use super::{label, underline};
use crate::Message;
use crate::config::{self, Config, Server};
use crate::rich::{self, RichVisuals};
use crate::theme::{self, Accent, Mode, Tokens, size};

/// The add/edit server form.
#[derive(Debug, Default)]
pub struct Editor {
    /// Which saved server this edits; `None` adds one.
    pub index: Option<usize>,
    pub label: String,
    pub host: String,
    pub port: String,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone)]
pub enum Field {
    Label,
    Host,
    Port,
    Username,
    Password,
}

impl Editor {
    pub fn add() -> Self {
        Self {
            port: config::DEFAULT_PORT.to_string(),
            ..Default::default()
        }
    }

    pub fn edit(index: usize, server: &Server) -> Self {
        Self {
            index: Some(index),
            label: server.label.clone(),
            host: server.host.clone(),
            port: server.port.to_string(),
            username: server.username.clone(),
            password: server.password.clone().unwrap_or_default(),
        }
    }

    pub fn set(&mut self, field: Field, value: String) {
        *match field {
            Field::Label => &mut self.label,
            Field::Host => &mut self.host,
            Field::Port => &mut self.port,
            Field::Username => &mut self.username,
            Field::Password => &mut self.password,
        } = value;
    }

    pub fn server(&self) -> Result<Server, String> {
        let host = self.host.trim();
        if host.is_empty() {
            return Err("the server needs an address".into());
        }
        let username = self.username.trim();
        if username.is_empty() {
            return Err("pick a username".into());
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
}

/// A page's body: padded like the nav row, scrolling if it must.
fn page<'a>(t: Tokens, content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scrollable(
        container(content)
            .padding(Padding {
                top: 24.0,
                right: super::INSET,
                bottom: 32.0,
                left: super::INSET,
            })
            .max_width(720),
    )
    .style(theme::scrollbar(t))
    .height(Fill)
    .into()
}

fn heading<'a>(t: Tokens, words: String) -> Element<'a, Message> {
    text(words)
        .size(28)
        .font(theme::display(Weight::ExtraBold))
        .style(theme::color(t.fg))
        .into()
}

fn body<'a>(t: Tokens, words: impl text::IntoFragment<'a>) -> text::Text<'a> {
    text(words)
        .size(size::BODY)
        .font(theme::ui(Weight::Normal))
        .style(theme::color(t.fg2))
}

fn pill_button<'a>(t: Tokens, words: &'a str, message: Message) -> Element<'a, Message> {
    button(
        text(words)
            .size(size::BODY)
            .font(theme::ui(Weight::Semibold)),
    )
    .on_press(message)
    .height(44)
    .padding([12, 22])
    .style(theme::pill(t))
    .into()
}

fn quiet_button<'a>(t: Tokens, words: &'a str, message: Message) -> Element<'a, Message> {
    button(text(words).size(size::BODY).font(theme::ui(Weight::Medium)))
        .on_press(message)
        .height(44)
        .padding([12, 10])
        .style(theme::quiet(t))
        .into()
}

pub fn servers<'a>(
    t: Tokens,
    config: &'a Config,
    connected: Option<&'a Server>,
) -> Element<'a, Message> {
    let mut list = Column::new().spacing(8);
    if config.servers.is_empty() {
        list = list.push(body(t, "no servers yet. add one to get talking."));
    }
    for (i, server) in config.servers.iter().enumerate() {
        let here = connected == Some(server);
        let name = text(server.label.to_lowercase())
            .size(24)
            .font(theme::display(Weight::ExtraBold))
            .wrapping(Wrapping::WordOrGlyph)
            .style(theme::color(t.fg));
        let detail = text(format!("{}@{}", server.username, server.address()))
            .size(size::EVENT)
            .style(theme::color(t.fg3));
        let action: Element<'a, Message> = if here {
            container(label("connected", t.fg2)).padding([0, 12]).into()
        } else {
            pill_button(t, "connect", Message::Connect(i))
        };
        list = list.push(
            row![
                column![name, detail].spacing(2).width(Fill),
                quiet_button(t, "edit", Message::EditServer(i)),
                quiet_button(t, "delete", Message::DeleteServer(i)),
                action,
            ]
            .spacing(6)
            .align_y(Center)
            .padding([10, 0]),
        );
    }
    page(
        t,
        column![list, row![pill_button(t, "add server", Message::AddServer)],].spacing(24),
    )
}

pub fn editor<'a>(t: Tokens, editor: &'a Editor) -> Element<'a, Message> {
    let field = |name: &'static str, value: &'a str, which: Field, secure: bool| {
        column![
            label(name, t.fg2),
            underline(
                t,
                text_input("", value)
                    .on_input(move |v| Message::Edit(which.clone(), v))
                    .on_submit(Message::SaveServer)
                    .secure(secure)
                    .size(size::BODY)
                    .padding(0)
                    .font(theme::ui(Weight::Normal))
                    .style(theme::underline_input(t)),
            ),
        ]
        .spacing(4)
    };
    page(
        t,
        column![
            heading(
                t,
                if editor.index.is_some() {
                    "edit server".into()
                } else {
                    "add server".into()
                }
            ),
            field("address", &editor.host, Field::Host, false),
            field("port", &editor.port, Field::Port, false),
            field("username", &editor.username, Field::Username, false),
            field(
                "password, if it has one",
                &editor.password,
                Field::Password,
                true
            ),
            field("name (optional)", &editor.label, Field::Label, false),
            row![
                pill_button(t, "save", Message::SaveServer),
                quiet_button(t, "cancel", Message::CancelEdit),
            ]
            .spacing(8),
        ]
        .spacing(20)
        .max_width(480),
    )
}

pub fn connecting<'a>(t: Tokens, server: &Server) -> Element<'a, Message> {
    container(
        column![
            heading(t, format!("connecting to {}", server.label.to_lowercase())),
            quiet_button(t, "cancel", Message::Cancel),
        ]
        .spacing(12)
        .align_x(Center),
    )
    .center(Fill)
    .into()
}

pub fn trust<'a>(
    t: Tokens,
    server: &Server,
    fingerprint: &Fingerprint,
    reason: &str,
) -> Element<'a, Message> {
    page(
        t,
        column![
            heading(t, format!("is this {}?", server.label.to_lowercase())),
            body(
                t,
                format!(
                    "the server's certificate isn't one blurt can verify ({reason}). \
                     if you trust it, blurt will remember it and warn you if it ever changes."
                )
            ),
            column![
                label("sha-256 fingerprint", t.fg2),
                text(fingerprint.to_string())
                    .font(Font::MONOSPACE)
                    .size(size::EVENT)
                    .style(theme::color(t.fg)),
            ]
            .spacing(6),
            row![
                pill_button(t, "trust and connect", Message::Trust),
                quiet_button(t, "cancel", Message::Cancel),
            ]
            .spacing(8),
        ]
        .spacing(20)
        .max_width(560),
    )
}

/// `rich_now`: whether this run asked for the GPU renderer, to say when a
/// changed rich visuals setting waits for a restart.
pub fn settings<'a>(t: Tokens, config: &'a Config, rich_now: bool) -> Element<'a, Message> {
    let a = &config.appearance;
    let choice = |words: &'static str, active: bool, message: Message| {
        button(text(words).size(20).font(theme::display(if active {
            Weight::ExtraBold
        } else {
            Weight::Medium
        })))
        .on_press(message)
        .height(44)
        .padding([8, 0])
        .style(theme::nav_word(t, active))
    };
    let swatch = |accent: Accent| {
        let tokens = Tokens::new(a.theme, accent);
        let active = a.accent == accent;
        button(
            row![
                container(space())
                    .width(22)
                    .height(22)
                    .style(move |_| container::Style {
                        background: Some(tokens.gradient()),
                        border: iced::Border {
                            color: if active {
                                t.fg
                            } else {
                                iced::Color::TRANSPARENT
                            },
                            width: 2.0,
                            radius: 999.0.into(),
                        },
                        ..Default::default()
                    }),
                text(accent.name()).size(20).font(theme::display(if active {
                    Weight::ExtraBold
                } else {
                    Weight::Medium
                })),
            ]
            .spacing(10)
            .align_y(Center),
        )
        .on_press(Message::SetAccent(accent))
        .height(44)
        .padding([8, 0])
        .style(theme::nav_word(t, active))
    };
    let section = |name: &'static str, choices: Element<'a, Message>| {
        column![label(name, t.fg2), choices].spacing(6)
    };
    page(
        t,
        column![
            section(
                "theme",
                row![
                    choice("dark", a.theme == Mode::Dark, Message::SetTheme(Mode::Dark)),
                    choice(
                        "light",
                        a.theme == Mode::Light,
                        Message::SetTheme(Mode::Light)
                    ),
                ]
                .spacing(24)
                .into()
            ),
            section(
                "accent",
                row(Accent::ALL.map(|accent| swatch(accent).into()))
                    .spacing(24)
                    .into()
            ),
            section(
                "motion",
                row![
                    choice("full", !a.reduce_motion, Message::SetReduceMotion(false)),
                    choice("reduced", a.reduce_motion, Message::SetReduceMotion(true)),
                ]
                .spacing(24)
                .into()
            ),
            column![
                section(
                    "rich visuals",
                    row![
                        choice(
                            "auto",
                            a.rich_visuals == RichVisuals::Auto,
                            Message::SetRichVisuals(RichVisuals::Auto)
                        ),
                        choice(
                            "on",
                            a.rich_visuals == RichVisuals::On,
                            Message::SetRichVisuals(RichVisuals::On)
                        ),
                        choice(
                            "off",
                            a.rich_visuals == RichVisuals::Off,
                            Message::SetRichVisuals(RichVisuals::Off)
                        ),
                    ]
                    .spacing(24)
                    .into()
                ),
                body(t, rich_note(a.rich_visuals, rich_now)),
            ]
            .spacing(6),
            body(t, "the accent only ever lights up for live audio."),
        ]
        .spacing(28),
    )
}

/// What the rich visuals setting does here, and whether it waits for a
/// restart.
fn rich_note(setting: RichVisuals, now: bool) -> String {
    if !cfg!(feature = "gpu") {
        return "this build of blurt draws with the cpu only.".into();
    }
    let mut note = String::from("draws with the gpu, for the room glow behind the controls.");
    if setting == RichVisuals::Auto {
        note += if rich::capable() {
            " auto: on here."
        } else {
            " auto: off here (it needs 32 gb of memory and a gpu)."
        };
    }
    if setting.wanted() != now {
        note += " restart blurt to apply.";
    }
    note
}

/// Shown above whatever page is up, until dismissed.
pub fn error_banner<'a>(t: Tokens, error: &'a str) -> Element<'a, Message> {
    container(
        row![
            text(error.to_owned())
                .size(size::BODY)
                .style(theme::color(t.fg))
                .width(Fill),
            quiet_button(t, "dismiss", Message::DismissError),
        ]
        .spacing(12)
        .align_y(Center),
    )
    .padding([0.0, super::INSET])
    .style(theme::fill(t.raise))
    .into()
}
