//! The voice screen: who's here, who's talking, the chat, and the controls.

use std::time::Instant;

use iced::font::Weight;
use iced::widget::text::{LineHeight, Wrapping};
use iced::widget::{
    Column, Row, button, column, container, mouse_area, pin, row, scrollable, slider, space, stack,
    text, text_input, tooltip,
};
use iced::{Bottom, Center, Element, Fill, Length, Padding, Shrink};
use mumble_protocol::state::{Channel, User};

use super::chrome::tip;
use super::{Layout, Page, Pivot, label, meter, underline};
use crate::icons::{self, Glyph};
use crate::room::Room;
use crate::theme::{self, ON_ACCENT, Tokens, alpha, mix, size};
use crate::transcript::{self, Entry};
use crate::{Message, talk};

/// Everything the voice screen draws from.
pub struct Voice<'a> {
    pub t: Tokens,
    pub room: &'a Room,
    pub now: Instant,
    pub width: f32,
    pub layout: Layout,
    pub reduce_motion: bool,
    pub volume: f32,
}

impl Voice<'_> {
    fn me(&self) -> Option<&User> {
        self.room.state.current_user()
    }

    fn channel(&self) -> Option<&Channel> {
        self.me()
            .and_then(|me| self.room.state.channels.get(&me.channel_id))
    }

    fn channel_name(&self) -> &str {
        self.channel().map_or("", |c| c.name.as_str())
    }

    /// Users in `channel`, by name, with you first.
    fn members(&self, channel: u32) -> Vec<&User> {
        let mut users: Vec<&User> = self
            .room
            .state
            .users
            .values()
            .filter(|u| u.channel_id == channel)
            .collect();
        users.sort_by_key(|u| (!u.is_me, u.name.to_lowercase()));
        users
    }

    /// 0 (idle) to 1 (talking), easing between the two.
    fn talking(&self, session: u32) -> f32 {
        self.room
            .talking
            .get(&session)
            .map_or(0.0, |a| a.interpolate(0.0, 1.0, self.now))
    }

    fn display_name<'b>(&self, user: &'b User) -> &'b str {
        if user.is_me { "you" } else { &user.name }
    }
}

pub fn view<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    match v.layout {
        Layout::Wide => wide(v),
        Layout::Compact => compact(v),
    }
}

fn wide<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let tree_width = (v.width * 0.28).clamp(260.0, 360.0);
    let main_width = v.width - tree_width;
    let hero = text(v.channel_name().to_owned())
        .size(theme::hero_size(main_width))
        .font(theme::display(Weight::ExtraBold))
        .line_height(LineHeight::Relative(0.92))
        .wrapping(Wrapping::None)
        .style(theme::color(t.fg));
    let main = column![
        container(hero).clip(true).width(Fill),
        talking_line(v, size::BODY),
        container(chat(v)).max_width(760).height(Fill),
    ]
    .spacing(18)
    .padding(Padding {
        top: 28.0,
        right: 32.0,
        bottom: 20.0,
        left: 12.0,
    })
    .width(Fill)
    .height(Fill);
    let body = row![
        container(scrollable(tree(v)).style(theme::scrollbar(t)).height(Fill))
            .width(tree_width)
            .padding(Padding {
                top: 28.0,
                right: 12.0,
                bottom: 40.0,
                left: 20.0,
            }),
        main,
    ]
    .height(Fill);
    let server = v.room.server.label.as_str();
    column![
        nav_row(t, Page::Voice, true, Some(server)),
        stack![decoration(t, server, v.width), body].height(Fill),
        control_bar(v),
    ]
    .into()
}

/// The server's name, huge and almost invisible, behind the top right.
fn decoration<'a>(t: Tokens, server: &str, width: f32) -> Element<'a, Message> {
    const SIZE: f32 = 220.0;
    let estimated = server.chars().count() as f32 * SIZE * 0.56;
    container(
        pin(text(server.to_lowercase())
            .size(SIZE)
            .font(theme::display(Weight::ExtraBold))
            .line_height(LineHeight::Relative(0.92))
            .wrapping(Wrapping::None)
            .style(theme::color(alpha(t.fg, 0.03))))
        .x(width - estimated * 0.7)
        .y(-SIZE * 0.18),
    )
    .width(Fill)
    .height(Fill)
    .clip(true)
    .into()
}

fn compact<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let hero = text(v.channel_name().to_owned())
        .size(size::HERO_COMPACT)
        .font(theme::display(Weight::ExtraBold))
        .line_height(LineHeight::Relative(0.92))
        .wrapping(Wrapping::None)
        .style(theme::color(t.fg));
    let pivots = Row::with_children(Pivot::ALL.map(|p| {
        button(
            text(p.name())
                .size(size::PIVOT_COMPACT)
                .font(theme::display(if p == v.room.pivot {
                    Weight::ExtraBold
                } else {
                    Weight::Medium
                }))
                .wrapping(Wrapping::None),
        )
        .on_press(Message::Pivot(p))
        .padding([4, 0])
        .height(44)
        .style(theme::nav_word(t, p == v.room.pivot))
        .into()
    }))
    .spacing(22);

    // The new tab slides in from the right and fades up.
    let shown = v.room.pivot_changed.map_or(1.0, |at| {
        progress(at, v.now, theme::motion::PIVOT, v.reduce_motion)
    });
    let tt = t.faded(shown);
    let shifted = Voice { t: tt, ..*v };
    let content: Element<'a, Message> = match v.room.pivot {
        Pivot::People => scrollable(people(&shifted))
            .style(theme::scrollbar(t))
            .into(),
        Pivot::Chat => chat(&shifted),
        Pivot::Channels => scrollable(channels(&shifted))
            .style(theme::scrollbar(t))
            .into(),
    };
    let content = container(content)
        .padding(Padding::ZERO.left(40.0 * (1.0 - shown)))
        .height(Fill);

    column![
        column![
            row![
                label(&v.room.server.label, t.fg2),
                space::horizontal(),
                page_links(t),
            ]
            .align_y(Center),
            container(hero).clip(true).width(Fill),
            talking_line(v, size::BODY),
            container(
                scrollable(pivots).direction(scrollable::Direction::Horizontal(
                    scrollable::Scrollbar::new().width(0).scroller_width(0)
                ))
            )
            .clip(true),
            content,
        ]
        .spacing(14)
        .padding(Padding {
            top: 4.0,
            right: 20.0,
            bottom: 16.0,
            left: 20.0,
        })
        .height(Fill),
        control_bar(v),
    ]
    .into()
}

/// In compact, the way to the other pages: two quiet words.
fn page_links<'a>(t: Tokens) -> Element<'a, Message> {
    row![
        button(text("servers").size(size::EVENT))
            .on_press(Message::Navigate(Page::Servers))
            .padding([12, 6])
            .style(theme::quiet(t)),
        button(text("settings").size(size::EVENT))
            .on_press(Message::Navigate(Page::Settings))
            .padding([12, 6])
            .style(theme::quiet(t)),
    ]
    .into()
}

/// `voice  servers  settings`, and the server's name at the right.
pub fn nav_row<'a>(
    t: Tokens,
    page: Page,
    connected: bool,
    server: Option<&str>,
) -> Element<'a, Message> {
    let pages: &[Page] = if connected {
        &[Page::Voice, Page::Servers, Page::Settings]
    } else {
        &[Page::Servers, Page::Settings]
    };
    let mut nav = Row::new().spacing(26).align_y(Center);
    for &p in pages {
        nav = nav.push(
            button(
                text(p.name())
                    .size(size::PIVOT)
                    .font(theme::display(if p == page {
                        Weight::ExtraBold
                    } else {
                        Weight::Medium
                    }))
                    .wrapping(Wrapping::None),
            )
            .on_press(Message::Navigate(p))
            .padding([4, 0])
            .height(44)
            .style(theme::nav_word(t, p == page)),
        );
    }
    let mut row = row![nav, space::horizontal()]
        .align_y(Center)
        .padding([0, 32]);
    if let Some(server) = server {
        row = row.push(label(server, t.fg2));
    }
    row.into()
}

/// The channel tree: every channel, and who's in it.
fn tree<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let here = v.me().map(|me| me.channel_id);
    let mut list = Column::new();
    for (depth, channel) in crate::channel_order(&v.room.state) {
        let indent = depth as f32 * 16.0;
        let current = here == Some(channel.channel_id);
        let marker = container(space())
            .width(3)
            .height(20)
            .style(theme::fill(if current {
                t.fg
            } else {
                iced::Color::TRANSPARENT
            }));
        let mut name_row = row![
            marker,
            text(channel.name.clone())
                .size(size::CHANNEL)
                .font(theme::ui(if current {
                    Weight::Bold
                } else {
                    Weight::Medium
                }))
                .wrapping(Wrapping::None)
                .style(theme::color(if current { t.fg } else { t.fg2 })),
        ]
        .spacing(10)
        .align_y(Center);
        if channel.is_enter_restricted {
            name_row = name_row.push(icons::standard(Glyph::Lock, 13.0, t.fg2));
        }
        list = list.push(
            row![
                space().width(indent),
                button(name_row.height(40))
                    .on_press(Message::Join(channel.channel_id))
                    .padding([0, 8])
                    .width(Fill)
                    .style(theme::row_button(t)),
            ]
            .height(40),
        );
        for user in v.members(channel.channel_id) {
            list = list.push(
                row![space().width(indent + 25.0), person(v, user, size::NAME),]
                    .height(30)
                    .align_y(Center)
                    .padding(Padding::ZERO.right(8)),
            );
        }
    }
    list.into()
}

/// A name, coloured by state, with mute and deafen icons at the right.
fn person<'a>(v: &Voice<'a>, user: &User, size: f32) -> Element<'a, Message> {
    let t = v.t;
    let muted = user.self_mute || user.mute || user.suppress;
    let deafened = user.self_deaf || user.deaf;
    let talking = v.talking(user.session);
    let idle = if muted || deafened { t.fg3 } else { t.fg_name };
    let mut row = row![
        text(v.display_name(user).to_owned())
            .size(size)
            .font(theme::ui(if talking > 0.5 {
                Weight::Bold
            } else {
                Weight::Medium
            }))
            .wrapping(Wrapping::None)
            .style(theme::color(mix(idle, t.accent_text, talking))),
        space::horizontal(),
    ]
    .spacing(8)
    .align_y(Center);
    if muted {
        row = row.push(icons::standard(Glyph::MicOff, 15.0, t.fg3));
    }
    if deafened {
        row = row.push(icons::standard(Glyph::HeadphonesOff, 15.0, t.fg3));
    }
    row.width(Fill).into()
}

/// Who's talking now, then who talked recently.
fn talking_line<'a>(v: &Voice<'a>, size: f32) -> Element<'a, Message> {
    let t = v.t;
    let line = v.room.talk.line(v.now);
    if line.is_empty() {
        return text("quiet").size(size).style(theme::color(t.fg3)).into();
    }
    let mut entries = Row::new().spacing(18);
    for spoke in line {
        let Some(user) = v.room.state.users.get(&spoke.session) else {
            continue;
        };
        let talking = v
            .talking(spoke.session)
            .max(if spoke.talking { 1.0 } else { 0.0 });
        let name = text(v.display_name(user).to_owned())
            .size(size)
            .font(theme::ui(if talking > 0.5 {
                Weight::Bold
            } else {
                Weight::Medium
            }))
            .style(theme::color(mix(t.fg2, t.accent_text, talking)));
        let entry = match spoke.ago {
            Some(ago) => row![
                name,
                text(talk::elapsed(ago))
                    .size(size::EVENT)
                    .style(theme::color(t.fg3)),
            ]
            .spacing(6)
            .align_y(Bottom),
            None => row![name],
        };
        entries = entries.push(entry);
    }
    entries.wrap().vertical_spacing(4).into()
}

/// The transcript, newest at the bottom, and the message box.
fn chat<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let mut log = Column::new().spacing(16).padding(Padding::ZERO.right(12));
    for entry in v.room.transcript.entries() {
        log = log.push(match entry {
            Entry::Message { author, time, body } => Element::from(
                column![
                    row![
                        text(author.clone().unwrap_or_else(|| "server".into()))
                            .size(size::AUTHOR)
                            .font(theme::ui(Weight::Bold))
                            .style(theme::color(t.fg)),
                        text(transcript::clock(time))
                            .size(size::TIME)
                            .style(theme::color(t.fg3)),
                    ]
                    .spacing(8)
                    .align_y(Bottom),
                    text(body.clone())
                        .size(size::BODY)
                        .line_height(LineHeight::Relative(1.5))
                        .font(theme::ui(Weight::Normal))
                        .style(theme::color(t.fg_body)),
                ]
                .spacing(2),
            ),
            Entry::Joined { name, time } => {
                text(format!("{name} joined · {}", transcript::clock(time)))
                    .size(size::EVENT)
                    .style(theme::color(t.fg3))
                    .into()
            }
            Entry::Left { name, time } => {
                text(format!("{name} left · {}", transcript::clock(time)))
                    .size(size::EVENT)
                    .style(theme::color(t.fg3))
                    .into()
            }
        });
    }
    let input = text_input(&format!("message {}", v.channel_name()), &v.room.draft)
        .on_input(Message::Draft)
        .on_submit(Message::SendChat)
        .size(size::BODY)
        .padding([0, 0])
        .font(theme::ui(Weight::Normal))
        .style(theme::underline_input(t));
    column![
        scrollable(log)
            .anchor_bottom()
            .height(Fill)
            .width(Fill)
            .style(theme::scrollbar(t)),
        row![
            underline(t, input),
            circle_button(t, Glyph::Send, false, 44.0, Message::SendChat, "send"),
        ]
        .spacing(12)
        .align_y(Center),
    ]
    .spacing(12)
    .height(Fill)
    .into()
}

/// Compact: the people in your channel, larger.
fn people<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let mut list = Column::new();
    if let Some(me) = v.me() {
        for user in v.members(me.channel_id) {
            list = list.push(
                container(person(v, user, size::NAME_COMPACT))
                    .height(40)
                    .center_y(40),
            );
        }
    }
    list.into()
}

/// Compact: channels, each with a grey line of who's there.
fn channels<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let here = v.me().map(|me| me.channel_id);
    let mut list = Column::new().spacing(4);
    for (_, channel) in crate::channel_order(&v.room.state) {
        let current = here == Some(channel.channel_id);
        let who: Vec<&str> = v
            .members(channel.channel_id)
            .into_iter()
            .map(|u| v.display_name(u))
            .collect();
        let mut title = row![
            text(channel.name.clone())
                .size(size::NAME_COMPACT)
                .font(theme::ui(if current {
                    Weight::Bold
                } else {
                    Weight::Medium
                }))
                .style(theme::color(if current { t.fg } else { t.fg2 })),
        ]
        .spacing(8)
        .align_y(Center);
        if channel.is_enter_restricted {
            title = title.push(icons::standard(Glyph::Lock, 13.0, t.fg2));
        }
        let mut entry = column![title].spacing(2);
        if !who.is_empty() {
            entry = entry.push(
                text(who.join(", "))
                    .size(size::EVENT)
                    .style(theme::color(t.fg3)),
            );
        }
        list = list.push(
            button(entry)
                .on_press(Message::Join(channel.channel_id))
                .padding([8, 8])
                .width(Fill)
                .style(theme::row_button(t)),
        );
    }
    list.into()
}

/// A circle with an icon. `on` fills it, e.g. for mute.
fn circle_button<'a>(
    t: Tokens,
    glyph: Glyph,
    on: bool,
    diameter: f32,
    message: Message,
    label: &'static str,
) -> Element<'a, Message> {
    let icon_size = (diameter * 0.4).round();
    tooltip(
        button(
            container(icons::standard(
                glyph,
                icon_size,
                if on { t.bg } else { t.fg },
            ))
            .center(diameter),
        )
        .on_press(message)
        .width(diameter)
        .height(diameter)
        .padding(0)
        .style(theme::circle(t, on)),
        tip(t, label),
        tooltip::Position::Top,
    )
    .into()
}

/// The push-to-talk pill: hold to talk, lit while you're transmitting.
fn talk_pill<'a>(v: &Voice<'a>, bars: usize, width: Length) -> Element<'a, Message> {
    let t = v.t;
    let c = v.room;
    let muted = c.muted || c.deafened;
    let live = c.live();
    let (words, color) = if muted {
        ("mic muted", t.fg3)
    } else if live {
        ("talking", ON_ACCENT)
    } else {
        ("hold to talk", t.fg)
    };
    let bar_color = if live { ON_ACCENT } else { t.line };
    let content = row![
        meter::meter(
            bars,
            c.mic_level,
            live && !v.reduce_motion,
            bar_color,
            v.now
        ),
        text(words)
            .size(size::BODY)
            .font(theme::ui(Weight::Bold))
            .wrapping(Wrapping::None)
            .style(theme::color(color)),
        space::horizontal(),
        label("space", if live { ON_ACCENT } else { t.fg3 }),
    ]
    .spacing(14)
    .align_y(Center);
    let background = if live { t.gradient() } else { t.raise.into() };
    let pill = container(content)
        .padding([0, 22])
        .height(56)
        .width(width)
        .center_y(56)
        .style(move |_| container::Style {
            background: Some(background),
            border: iced::Border {
                color: if live {
                    iced::Color::TRANSPARENT
                } else {
                    t.line
                },
                width: 1.5,
                radius: 28.0.into(),
            },
            ..Default::default()
        });
    mouse_area(pill)
        .on_press(Message::PushToTalk(true))
        .on_release(Message::PushToTalk(false))
        .on_exit(Message::PushToTalk(false))
        .interaction(iced::mouse::Interaction::Pointer)
        .into()
}

fn mute_buttons<'a>(v: &Voice<'a>) -> (Element<'a, Message>, Element<'a, Message>) {
    let t = v.t;
    let muted = v.room.muted || v.room.deafened;
    let deafened = v.room.deafened;
    (
        circle_button(
            t,
            if muted { Glyph::MicOff } else { Glyph::Mic },
            muted,
            56.0,
            Message::ToggleMute,
            if muted { "unmute" } else { "mute" },
        ),
        circle_button(
            t,
            if deafened {
                Glyph::HeadphonesOff
            } else {
                Glyph::Headphones
            },
            deafened,
            56.0,
            Message::ToggleDeafen,
            if deafened { "undeafen" } else { "deafen" },
        ),
    )
}

fn volume_slider<'a>(v: &Voice<'a>, width: impl Into<Length>) -> Element<'a, Message> {
    slider(0.0..=1.0, v.volume, Message::Volume)
        .on_release(Message::SaveVolume)
        .step(0.01f32)
        .width(width)
        .style(theme::accent_slider(v.t))
        .into()
}

/// The 2 px accent strip along the bar's top.
fn strip<'a>(t: Tokens) -> Element<'a, Message> {
    container(space())
        .width(Fill)
        .height(2)
        .style(theme::background(t.gradient()))
        .into()
}

fn control_bar<'a>(v: &Voice<'a>) -> Element<'a, Message> {
    let t = v.t;
    let (mute, deafen) = mute_buttons(v);
    let bar = match v.layout {
        Layout::Wide => {
            let leave = button(
                container(
                    row![
                        icons::standard(Glyph::Leave, 16.0, t.fg),
                        text("leave")
                            .size(size::BODY)
                            .font(theme::ui(Weight::Semibold)),
                    ]
                    .spacing(8)
                    .align_y(Center),
                )
                .center_y(Fill),
            )
            .on_press(Message::Disconnect)
            .padding([14, 20])
            .style(theme::pill(t));
            Element::from(
                row![
                    row![mute, deafen].spacing(12),
                    talk_pill(v, 12, Length::Fixed(300.0)),
                    space::horizontal(),
                    icons::standard(Glyph::Volume, 18.0, t.fg2),
                    label("output", t.fg2),
                    volume_slider(v, 140),
                    space().width(8),
                    container(leave).center_y(56),
                ]
                .spacing(16)
                .align_y(Center)
                .height(56),
            )
        }
        Layout::Compact => {
            let mut bar = Column::new().spacing(14);
            if v.room.volume_open {
                bar = bar.push(
                    row![
                        icons::standard(Glyph::Volume, 18.0, t.fg2),
                        volume_slider(v, Fill),
                    ]
                    .spacing(14)
                    .align_y(Center),
                );
            }
            bar = bar.push(talk_pill(v, 8, Length::Fill));
            bar = bar.push(
                row![
                    mute,
                    space::horizontal(),
                    deafen,
                    space::horizontal(),
                    circle_button(
                        t,
                        Glyph::Volume,
                        v.room.volume_open,
                        56.0,
                        Message::ToggleVolume,
                        "volume"
                    ),
                    space::horizontal(),
                    circle_button(t, Glyph::Leave, false, 56.0, Message::Disconnect, "leave"),
                ]
                .align_y(Center),
            );
            bar.into()
        }
    };
    column![
        strip(t),
        container(bar)
            .padding([16, 28])
            .width(Fill)
            .height(Shrink)
            .style(theme::fill(t.bar)),
    ]
    .into()
}

/// How far an animation that started `at` is, eased, from 0 to 1.
pub fn progress(
    at: Instant,
    now: Instant,
    duration: std::time::Duration,
    reduce_motion: bool,
) -> f32 {
    if reduce_motion {
        return 1.0;
    }
    let t = now.saturating_duration_since(at).as_secs_f32() / duration.as_secs_f32();
    theme::ease(t)
}
