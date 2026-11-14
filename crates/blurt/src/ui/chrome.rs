//! The window's own frame: blurt draws no system decorations, so it brings
//! its title bar (drag area plus thin glyphs) and invisible resize edges.

use iced::widget::{button, column, mouse_area, row, space, stack, tooltip};
use iced::window::Direction;
use iced::{Element, Fill, mouse};

use crate::Message;
use crate::icons::{self, Glyph};
use crate::theme::{self, Tokens};

pub const TITLE_BAR: f32 = 36.0;
/// How far in from the window's edge resizing can start.
const EDGE: f32 = 6.0;

/// An empty bar that drags the window, with minimize, maximize and close at
/// the right. `before` sits at its left, e.g. the compact layout's label.
pub fn title_bar<'a>(t: Tokens, before: Option<Element<'a, Message>>) -> Element<'a, Message> {
    let glyph = |glyph: Glyph, message: Message, label: &'static str| {
        tooltip(
            button(icons::icon(glyph, 12.0, 1.4, t.fg2))
                .on_press(message)
                .width(44)
                .height(32)
                .padding([10, 16])
                .style(theme::glyph(t)),
            tip(t, label),
            tooltip::Position::Bottom,
        )
    };
    let mut inside = row![]
        .height(TITLE_BAR)
        .align_y(iced::Center)
        .padding([0, 16]);
    if let Some(before) = before {
        inside = inside.push(before);
    }
    let drag = mouse_area(inside.push(space::horizontal()))
        .on_press(Message::Drag)
        .on_double_click(Message::ToggleMaximize);
    row![
        drag,
        glyph(Glyph::Minimize, Message::Minimize, "minimize"),
        glyph(Glyph::Maximize, Message::ToggleMaximize, "maximize"),
        glyph(Glyph::Close, Message::CloseWindow, "close"),
    ]
    .height(TITLE_BAR)
    .align_y(iced::Center)
    .padding(iced::Padding::ZERO.right(4))
    .into()
}

/// A tooltip's words: the label for an icon-only button.
pub fn tip<'a>(t: Tokens, words: &'static str) -> Element<'a, Message> {
    iced::widget::container(
        iced::widget::text(words)
            .size(theme::size::TIME)
            .font(theme::ui(iced::font::Weight::Medium))
            .style(theme::color(t.fg)),
    )
    .padding([4, 8])
    .style(move |_| iced::widget::container::Style {
        background: Some(t.raise.into()),
        border: iced::Border {
            color: t.line,
            width: 1.0,
            radius: 2.0.into(),
        },
        ..Default::default()
    })
    .into()
}

/// `content`, with resize handles along the window's edges on top.
pub fn with_resize_edges<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    let handle = |direction: Direction, interaction: mouse::Interaction| {
        mouse_area(space().width(Fill).height(Fill))
            .on_press(Message::Resize(direction))
            .interaction(interaction)
    };
    let corner = |direction, interaction| {
        iced::widget::container(handle(direction, interaction))
            .width(EDGE * 2.0)
            .height(EDGE)
    };
    let edges = column![
        row![
            corner(
                Direction::NorthWest,
                mouse::Interaction::ResizingDiagonallyDown
            ),
            handle(Direction::North, mouse::Interaction::ResizingVertically),
            corner(
                Direction::NorthEast,
                mouse::Interaction::ResizingDiagonallyUp
            ),
        ]
        .height(EDGE),
        row![
            iced::widget::container(handle(
                Direction::West,
                mouse::Interaction::ResizingHorizontally
            ))
            .width(EDGE),
            space::horizontal(),
            iced::widget::container(handle(
                Direction::East,
                mouse::Interaction::ResizingHorizontally
            ))
            .width(EDGE),
        ]
        .height(Fill),
        row![
            corner(
                Direction::SouthWest,
                mouse::Interaction::ResizingDiagonallyUp
            ),
            handle(Direction::South, mouse::Interaction::ResizingVertically),
            corner(
                Direction::SouthEast,
                mouse::Interaction::ResizingDiagonallyDown
            ),
        ]
        .height(EDGE),
    ];
    stack![content, edges].into()
}
