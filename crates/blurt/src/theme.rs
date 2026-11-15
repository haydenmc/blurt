//! The design tokens: colours, type, shape and motion, all in one place.
//!
//! Everything is greyscale except the accent, which only ever means live
//! audio. Widgets are styled through closures that read a copied [`Tokens`],
//! never through hard-coded colours.

use std::time::Duration;

use iced::font::{Family, Weight};
use iced::gradient::Linear;
use iced::widget::{button, container, scrollable, slider, text, text_input};
use iced::{Background, Border, Color, Degrees, Font, Shadow};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Dark,
    Light,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Zune,
    Ice,
    Acid,
}

impl Accent {
    pub const ALL: [Accent; 3] = [Accent::Zune, Accent::Ice, Accent::Acid];

    pub fn name(self) -> &'static str {
        match self {
            Accent::Zune => "zune",
            Accent::Ice => "ice",
            Accent::Acid => "acid",
        }
    }

    /// The gradient's two ends.
    pub fn ends(self) -> (Color, Color) {
        let (a1, a2, _) = self.colors();
        (a1, a2)
    }

    /// The gradient's two ends, and a darker shade for accent text on light
    /// backgrounds.
    fn colors(self) -> (Color, Color, Color) {
        match self {
            Accent::Zune => (hex(0xFF2D95), hex(0xFF7A1A), hex(0xC4005C)),
            Accent::Ice => (hex(0x22D3EE), hex(0x8B5CF6), hex(0x0E7490)),
            Accent::Acid => (hex(0xC6FF1A), hex(0x14E3A1), hex(0x3F6B00)),
        }
    }
}

/// Text on the accent gradient: dark in both modes.
pub const ON_ACCENT: Color = hex(0x0A0A0C);

const fn hex(rgb: u32) -> Color {
    Color::from_rgb8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

/// Every colour the UI uses.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Tokens {
    pub mode: Mode,
    /// Window background.
    pub bg: Color,
    /// Control bar.
    pub bar: Color,
    /// Hover fills, input backgrounds.
    pub raise: Color,
    /// Button outlines.
    pub line: Color,
    /// Rare separators.
    pub hairline: Color,
    /// Primary text, active nav.
    pub fg: Color,
    /// Chat message text.
    pub fg_body: Color,
    /// Idle names in lists.
    pub fg_name: Color,
    /// Labels, secondary text.
    pub fg2: Color,
    /// Timestamps, muted names, placeholders.
    pub fg3: Color,
    /// Inactive pivot words (30 px and up only).
    pub nav_idle: Color,
    /// Gradient start; accent text on dark.
    pub a1: Color,
    /// Gradient end.
    pub a2: Color,
    /// Accent for text, readable on `bg`.
    pub accent_text: Color,
}

impl Tokens {
    pub fn new(mode: Mode, accent: Accent) -> Self {
        let (a1, a2, on_light) = accent.colors();
        match mode {
            Mode::Dark => Self {
                mode,
                bg: hex(0x0A0A0C),
                bar: hex(0x0D0D10),
                raise: hex(0x17171C),
                line: hex(0x3A3A43),
                hairline: hex(0x1D1D23),
                fg: hex(0xF5F5F7),
                fg_body: hex(0xDCDCE2),
                fg_name: hex(0xC9C9D1),
                fg2: hex(0xA1A1AA),
                fg3: hex(0x8A8A93),
                nav_idle: hex(0x77777F),
                a1,
                a2,
                accent_text: a1,
            },
            Mode::Light => Self {
                mode,
                bg: hex(0xF5F5F2),
                bar: hex(0xFFFFFF),
                raise: hex(0xFFFFFF),
                line: hex(0xD0D0CA),
                hairline: hex(0xE3E3DF),
                fg: hex(0x0E0E10),
                fg_body: hex(0x0E0E10),
                fg_name: hex(0x0E0E10),
                fg2: hex(0x55555C),
                fg3: hex(0x6B6B72),
                nav_idle: hex(0x8A8A90),
                a1,
                a2,
                accent_text: on_light,
            },
        }
    }

    /// The accent gradient: linear, 100°, a1 → a2.
    pub fn gradient(&self) -> Background {
        Background::Gradient(
            Linear::new(Degrees(100.0))
                .add_stop(0.0, self.a1)
                .add_stop(1.0, self.a2)
                .into(),
        )
    }

    /// The same tokens with all text and lines faded toward the background,
    /// `shown` from 0 (invisible) to 1 (as is). For fading content in.
    pub fn faded(&self, shown: f32) -> Tokens {
        if shown >= 1.0 {
            return *self;
        }
        let f = |c: Color| mix(self.bg, c, shown);
        Tokens {
            line: f(self.line),
            hairline: f(self.hairline),
            fg: f(self.fg),
            fg_body: f(self.fg_body),
            fg_name: f(self.fg_name),
            fg2: f(self.fg2),
            fg3: f(self.fg3),
            nav_idle: f(self.nav_idle),
            accent_text: f(self.accent_text),
            ..*self
        }
    }

    /// For iced's own defaults (scrollbars, selection) to fit in.
    pub fn iced_theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "blurt",
            iced::theme::Palette {
                background: self.bg,
                text: self.fg,
                primary: self.a1,
                success: self.a1,
                warning: self.fg2,
                danger: self.fg,
            },
        )
    }
}

/// Mixes `a` toward `b` by `t` (0 to 1).
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color {
        r: a.r + (b.r - a.r) * t,
        g: a.g + (b.g - a.g) * t,
        b: a.b + (b.b - a.b) * t,
        a: a.a + (b.a - a.a) * t,
    }
}

pub fn alpha(color: Color, a: f32) -> Color {
    Color { a, ..color }
}

// Type. Inter Tight for display, Inter for the UI, both bundled (variable
// fonts, so every weight comes from one file; OFL, see assets/fonts).

/// The font files, to load at startup.
pub const FONTS: [&[u8]; 2] = [
    include_bytes!("../assets/fonts/Inter.ttf"),
    include_bytes!("../assets/fonts/InterTight.ttf"),
];

pub const DISPLAY: &str = "Inter Tight";
pub const UI: &str = "Inter";

pub fn display(weight: Weight) -> Font {
    Font {
        family: Family::Name(DISPLAY),
        weight,
        ..Font::DEFAULT
    }
}

pub fn ui(weight: Weight) -> Font {
    Font {
        family: Family::Name(UI),
        weight,
        ..Font::DEFAULT
    }
}

pub mod size {
    pub const HERO_MIN: f32 = 48.0;
    pub const HERO_MAX: f32 = 104.0;
    pub const HERO_COMPACT: f32 = 76.0;
    pub const PIVOT: f32 = 30.0;
    pub const PIVOT_COMPACT: f32 = 32.0;
    pub const CHANNEL: f32 = 16.0;
    pub const NAME: f32 = 15.0;
    pub const NAME_COMPACT: f32 = 20.0;
    pub const BODY: f32 = 15.0;
    pub const AUTHOR: f32 = 14.0;
    pub const TIME: f32 = 12.0;
    pub const EVENT: f32 = 13.0;
    pub const LABEL: f32 = 12.0;
}

/// The hero (channel name) size for a body of `width`: 48 to 104 px.
pub fn hero_size(width: f32) -> f32 {
    (width * 0.09).clamp(size::HERO_MIN, size::HERO_MAX)
}

// Motion.

pub mod motion {
    use super::Duration;
    pub const STATE: Duration = Duration::from_millis(140);
    pub const PIVOT: Duration = Duration::from_millis(220);
    pub const REFLOW: Duration = Duration::from_millis(260);
}

/// `cubic-bezier(0.16, 1, 0.3, 1)`: the one easing curve, a quick start
/// that settles gently.
pub fn ease(t: f32) -> f32 {
    const X1: f32 = 0.16;
    const Y1: f32 = 1.0;
    const X2: f32 = 0.3;
    const Y2: f32 = 1.0;
    let t = t.clamp(0.0, 1.0);
    let bezier = |p1: f32, p2: f32, s: f32| {
        let u = 1.0 - s;
        3.0 * u * u * s * p1 + 3.0 * u * s * s * p2 + s * s * s
    };
    // Solve x(s) = t by bisection; x is monotonic for these control points.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..20 {
        let mid = (lo + hi) / 2.0;
        if bezier(X1, X2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bezier(Y1, Y2, (lo + hi) / 2.0)
}

/// For [`iced::Animation`].
pub const EASING: iced::animation::Easing = iced::animation::Easing::Custom(ease);

// Styles. Each takes the tokens by value so closures can own them.

fn no_border() -> Border {
    Border::default()
}

fn outline(color: Color, width: f32, radius: f32) -> Border {
    Border {
        color,
        width,
        radius: radius.into(),
    }
}

/// Plain words in the nav row.
pub fn nav_word(t: Tokens, active: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| {
        let color = match (active, status) {
            (true, _) => t.fg,
            (false, button::Status::Hovered | button::Status::Pressed) => t.fg,
            (false, _) => t.nav_idle,
        };
        button::Style {
            background: None,
            text_color: color,
            border: no_border(),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// A list row: no fill until hovered.
pub fn row_button(t: Tokens) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered => Some(t.raise.into()),
            button::Status::Pressed => Some(mix(t.raise, t.line, 0.5).into()),
            _ => None,
        },
        text_color: t.fg,
        border: outline(Color::TRANSPARENT, 0.0, 2.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Circle icon buttons (mute, deafen, send...). `on` fills them solid.
pub fn circle(t: Tokens, on: bool) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| {
        let (background, border) = if on {
            (Some(t.fg.into()), outline(t.fg, 1.5, 999.0))
        } else {
            let fill = match status {
                button::Status::Hovered => Some(t.raise.into()),
                button::Status::Pressed => Some(mix(t.raise, t.line, 0.5).into()),
                _ => None,
            };
            let line = match status {
                button::Status::Hovered | button::Status::Pressed => mix(t.line, t.fg2, 0.5),
                _ => t.line,
            };
            (fill, outline(line, 1.5, 999.0))
        };
        button::Style {
            background,
            text_color: if on { t.bg } else { t.fg },
            border,
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// Outlined pills with words (leave, connect).
pub fn pill(t: Tokens) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered => Some(t.raise.into()),
            button::Status::Pressed => Some(mix(t.raise, t.line, 0.5).into()),
            _ => None,
        },
        text_color: match status {
            button::Status::Disabled => t.fg3,
            _ => t.fg,
        },
        border: outline(t.line, 1.5, 999.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Words that act, without a box (edit, delete, cancel).
pub fn quiet(t: Tokens) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: None,
        text_color: match status {
            button::Status::Hovered | button::Status::Pressed => t.fg,
            _ => t.fg2,
        },
        border: no_border(),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Window glyphs in the title bar.
pub fn glyph(t: Tokens) -> impl Fn(&iced::Theme, button::Status) -> button::Style {
    move |_, status| button::Style {
        background: match status {
            button::Status::Hovered => Some(t.raise.into()),
            button::Status::Pressed => Some(mix(t.raise, t.line, 0.5).into()),
            _ => None,
        },
        text_color: t.fg2,
        border: outline(Color::TRANSPARENT, 0.0, 2.0),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A transparent input with only a bottom line. iced draws borders all
/// round, so the line is drawn by the caller (see `ui::underline`); the
/// input itself has none.
pub fn underline_input(
    t: Tokens,
) -> impl Fn(&iced::Theme, text_input::Status) -> text_input::Style {
    move |_, _| text_input::Style {
        background: Color::TRANSPARENT.into(),
        border: no_border(),
        icon: t.fg2,
        placeholder: t.fg3,
        value: t.fg,
        selection: alpha(t.a1, 0.35),
    }
}

pub fn fill(color: Color) -> impl Fn(&iced::Theme) -> container::Style {
    move |_| container::Style {
        background: Some(color.into()),
        ..container::Style::default()
    }
}

pub fn background(background: Background) -> impl Fn(&iced::Theme) -> container::Style {
    move |_| container::Style {
        background: Some(background),
        ..container::Style::default()
    }
}

pub fn color(color: Color) -> impl Fn(&iced::Theme) -> text::Style {
    move |_| text::Style { color: Some(color) }
}

/// The accent slider: accent rail up to the handle, line colour after.
pub fn accent_slider(t: Tokens) -> impl Fn(&iced::Theme, slider::Status) -> slider::Style {
    move |_, status| slider::Style {
        rail: slider::Rail {
            backgrounds: (t.a1.into(), t.line.into()),
            width: 2.0,
            border: outline(Color::TRANSPARENT, 0.0, 1.0),
        },
        handle: slider::Handle {
            shape: slider::HandleShape::Circle {
                radius: match status {
                    slider::Status::Active => 6.0,
                    _ => 7.0,
                },
            },
            background: t.fg.into(),
            border_width: 0.0,
            border_color: Color::TRANSPARENT,
        },
    }
}

/// Scrollbars that stay out of the way: thin, in line colour.
pub fn scrollbar(t: Tokens) -> impl Fn(&iced::Theme, scrollable::Status) -> scrollable::Style {
    move |theme, status| {
        let mut style = scrollable::default(theme, status);
        for rail in [&mut style.vertical_rail, &mut style.horizontal_rail] {
            rail.background = None;
            rail.border = no_border();
            rail.scroller.border = outline(Color::TRANSPARENT, 0.0, 999.0);
            rail.scroller.background = match status {
                scrollable::Status::Active { .. } => alpha(t.line, 0.5).into(),
                _ => t.line.into(),
            };
        }
        style
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(c: Color) -> f32 {
        let channel = |v: f32| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r) + 0.7152 * channel(c.g) + 0.0722 * channel(c.b)
    }

    fn contrast(a: Color, b: Color) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn accent_text_is_readable_everywhere() {
        for mode in [Mode::Dark, Mode::Light] {
            for accent in Accent::ALL {
                let t = Tokens::new(mode, accent);
                let ratio = contrast(t.accent_text, t.bg);
                assert!(ratio >= 4.5, "{accent:?} on {mode:?}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn body_text_is_readable() {
        for mode in [Mode::Dark, Mode::Light] {
            let t = Tokens::new(mode, Accent::Zune);
            for (name, fg) in [("fg", t.fg), ("fg2", t.fg2), ("fg3", t.fg3)] {
                let ratio = contrast(fg, t.bg);
                assert!(ratio >= 4.5, "{name} on {mode:?}: {ratio:.2}");
            }
        }
    }

    #[test]
    fn easing_starts_fast_and_settles() {
        assert!(ease(0.0).abs() < 1e-3);
        assert!((ease(1.0) - 1.0).abs() < 1e-3);
        // Most of the movement happens early.
        assert!(ease(0.25) > 0.6, "{}", ease(0.25));
        let mut last = 0.0;
        for i in 1..=20 {
            let v = ease(i as f32 / 20.0);
            assert!(v >= last);
            last = v;
        }
    }

    #[test]
    fn hero_is_clamped() {
        assert_eq!(hero_size(100.0), size::HERO_MIN);
        assert_eq!(hero_size(5000.0), size::HERO_MAX);
    }
}
