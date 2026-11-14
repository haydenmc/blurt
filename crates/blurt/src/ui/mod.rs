//! Views. Each takes the [`Tokens`] it styles with; none hold state.

pub mod chrome;
pub mod meter;
pub mod pages;
pub mod voice;

use iced::widget::{column, container, rule, text};
use iced::{Element, Fill};

use crate::theme::{self, Tokens};

/// Below this window width, the compact layout.
pub const BREAKPOINT: f32 = 720.0;
/// So a window resized right at the breakpoint doesn't flip back and forth.
const HYSTERESIS: f32 = 8.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Wide,
    Compact,
}

impl Layout {
    pub fn for_width(width: f32) -> Self {
        if width >= BREAKPOINT {
            Layout::Wide
        } else {
            Layout::Compact
        }
    }

    /// The layout after resizing to `width`, given the current one.
    pub fn resized(self, width: f32) -> Self {
        match self {
            Layout::Wide if width < BREAKPOINT - HYSTERESIS => Layout::Compact,
            Layout::Compact if width >= BREAKPOINT + HYSTERESIS => Layout::Wide,
            same => same,
        }
    }
}

/// The top-level pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Voice,
    Servers,
    Settings,
}

impl Page {
    pub fn name(self) -> &'static str {
        match self {
            Page::Voice => "voice",
            Page::Servers => "servers",
            Page::Settings => "settings",
        }
    }
}

/// The compact layout's tabs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pivot {
    People,
    Chat,
    Channels,
}

impl Pivot {
    pub const ALL: [Pivot; 3] = [Pivot::People, Pivot::Chat, Pivot::Channels];

    pub fn name(self) -> &'static str {
        match self {
            Pivot::People => "people",
            Pivot::Chat => "chat",
            Pivot::Channels => "channels",
        }
    }
}

/// A small-caps label: 12 px, 600, in capitals.
pub fn label<'a, Message: 'a>(words: &str, color: iced::Color) -> Element<'a, Message> {
    text(words.to_uppercase())
        .size(theme::size::LABEL)
        .font(theme::ui(iced::font::Weight::Semibold))
        .style(theme::color(color))
        .into()
}

/// An input with only a bottom border, in line colour.
pub fn underline<'a, Message: 'a>(
    t: Tokens,
    input: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    column![
        container(input).height(43).center_y(43),
        rule::horizontal(1).style(move |_| rule::Style {
            color: t.line,
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
            snap: true,
        }),
    ]
    .width(Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_switches_once_across_the_breakpoint() {
        let mut layout = Layout::for_width(900.0);
        assert_eq!(layout, Layout::Wide);
        // Dragging slowly around the line doesn't flicker.
        for width in [719.0, 716.0, 724.0, 719.0] {
            layout = layout.resized(width);
            assert_eq!(layout, Layout::Wide, "{width}");
        }
        layout = layout.resized(700.0);
        assert_eq!(layout, Layout::Compact);
        for width in [720.0, 726.0, 712.0] {
            layout = layout.resized(width);
            assert_eq!(layout, Layout::Compact, "{width}");
        }
        assert_eq!(layout.resized(740.0), Layout::Wide);
    }
}
