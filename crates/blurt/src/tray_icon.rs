//! The tray icon, drawn here rather than shipped as images: blurt's app
//! icon (packaging/icons), a speech bubble with a sound wave, in a few
//! states. Only live audio gets colour, as everywhere in blurt: connected,
//! the bubble is white (rimmed, so it reads on a light panel); while
//! you're being heard, it lights up in the accent. Disconnected, it's grey.
//! Muted or deafened, it's struck through: a dimmed wave in a white bubble,
//! or (deaf) a grey one. The app icon itself, in its own colours, is
//! [`app_icon`].

use std::sync::OnceLock;

use blurt_platform::tray::Image;

use crate::theme::Accent;
use tiny_skia::{
    Color, FillRule, GradientStop, LinearGradient, Paint, PathBuilder, Pixmap, Point, Rect, Shader,
    SpreadMode, Stroke, Transform,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    Disconnected,
    Connected,
    /// Connected, and your voice is going out.
    Transmitting,
    Muted,
    Deafened,
}

impl Look {
    pub fn new(connected: bool, transmitting: bool, muted: bool, deafened: bool) -> Self {
        match (connected, muted, deafened) {
            (false, ..) => Look::Disconnected,
            (true, _, true) => Look::Deafened,
            (true, true, false) => Look::Muted,
            (true, false, false) if transmitting => Look::Transmitting,
            (true, false, false) => Look::Connected,
        }
    }

    /// The icon at the sizes trays commonly use, in `accent` (which only
    /// transmitting shows); drawn once each.
    pub fn images(self, accent: Accent) -> &'static [Image] {
        const LOOKS: usize = 5;
        static IMAGES: [[OnceLock<Vec<Image>>; Accent::ALL.len()]; LOOKS] =
            [const { [const { OnceLock::new() }; Accent::ALL.len()] }; LOOKS];
        let a = Accent::ALL.iter().position(|&x| x == accent).unwrap_or(0);
        IMAGES[self as usize][a].get_or_init(|| {
            SIZES
                .iter()
                .map(|&size| draw(Style::of(self, accent), size))
                .collect()
        })
    }
}

// Windows asks for its small icon size times the display scale (125%:
// 20, 150%: 24, ...): drawn at each, it never scales one itself.
#[cfg(windows)]
const SIZES: [u32; 7] = [16, 20, 24, 32, 40, 48, 64];
#[cfg(not(windows))]
const SIZES: [u32; 4] = [16, 22, 32, 64];

/// The app icon's design grid.
const GRID: f32 = 64.0;

/// The app icon's gradient, left to right.
const LOGO: (u32, u32) = (0xff2d95, 0xff7a1a);

fn rgb(hex: u32) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 255)
}

fn from_iced(c: iced::Color) -> Color {
    Color::from_rgba(c.r, c.g, c.b, c.a).expect("a valid colour")
}

/// Left to right across the bubble, as in the app icon's SVG.
fn gradient(from: Color, to: Color) -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(1.83, 0.0),
        Point::from_xy(62.40, 0.0),
        vec![GradientStop::new(0.0, from), GradientStop::new(1.0, to)],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("a valid gradient")
}

/// What a drawing of the icon paints.
struct Style {
    bubble: Shader<'static>,
    wave: Color,
    /// A thin dark edge, for a light bubble on a light panel.
    rim: bool,
    /// Struck through, as muted or deafened.
    struck: bool,
}

impl Style {
    fn of(look: Look, accent: Accent) -> Self {
        let white = |alpha| Color::from_rgba(1.0, 1.0, 1.0, alpha).unwrap();
        let dark = |alpha| {
            let mut c = from_iced(crate::theme::ON_ACCENT);
            c.set_alpha(alpha);
            c
        };
        let grey = || Shader::SolidColor(rgb(0x7c828d));
        let light = || Shader::SolidColor(rgb(0xf4f4f6));
        let struck = matches!(look, Look::Muted | Look::Deafened);
        let (bubble, wave) = match look {
            Look::Disconnected => (grey(), white(1.0)),
            Look::Connected => (light(), dark(1.0)),
            // Dark on the accent, as text is in the app.
            Look::Transmitting => {
                let (a1, a2) = accent.ends();
                (gradient(from_iced(a1), from_iced(a2)), dark(1.0))
            }
            // Dimmed while it isn't going anywhere.
            Look::Muted => (light(), dark(0.45)),
            Look::Deafened => (grey(), white(0.45)),
        };
        Style {
            bubble,
            wave,
            rim: matches!(look, Look::Connected | Look::Muted),
            struck,
        }
    }
}

/// The app icon, `size` px square, in its own colours.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn app_icon(size: u32) -> Image {
    let style = Style {
        bubble: gradient(rgb(LOGO.0), rgb(LOGO.1)),
        wave: rgb(0xffffff),
        rim: false,
        struck: false,
    };
    draw(style, size)
}

fn draw(style: Style, size: u32) -> Image {
    let mut pixmap = Pixmap::new(size, size).expect("a non-empty icon");
    let scale = size as f32 / GRID;
    let transform = Transform::from_scale(scale, scale);
    let paint = |shader| Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    };

    let bubble = bubble();
    pixmap.fill_path(
        &bubble,
        &paint(style.bubble),
        FillRule::Winding,
        transform,
        None,
    );
    if style.rim {
        // A device pixel wide at any size.
        let rim = Stroke {
            width: 1.0 / scale,
            ..Stroke::default()
        };
        let edge = paint(Shader::SolidColor(Color::from_rgba8(0, 0, 0, 90)));
        pixmap.stroke_path(&bubble, &edge, &rim, transform, None);
    }

    let wave = paint(Shader::SolidColor(style.wave));
    for (x, y, h) in WAVE {
        if let Some(bar) = rounded_rect(x, y, BAR, h, BAR / 2.0) {
            pixmap.fill_path(&bar, &wave, FillRule::Winding, transform, None);
        }
    }

    // The strike: red, edged in white so it reads on any panel.
    if style.struck {
        let mut line = PathBuilder::new();
        line.move_to(11.0, 11.0);
        line.line_to(53.0, 53.0);
        let line = line.finish().expect("a line");
        let stroke = |width| Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        let edge = paint(Shader::SolidColor(rgb(0xffffff)));
        pixmap.stroke_path(&line, &edge, &stroke(11.0), transform, None);
        let red = paint(Shader::SolidColor(rgb(0xe5484d)));
        pixmap.stroke_path(&line, &red, &stroke(6.0), transform, None);
    }

    // tiny-skia stores premultiplied RGBA; the tray wants straight ARGB.
    let argb = pixmap
        .pixels()
        .iter()
        .flat_map(|p| {
            let c = p.demultiply();
            [c.alpha(), c.red(), c.green(), c.blue()]
        })
        .collect();
    Image {
        width: size,
        height: size,
        argb,
    }
}

/// The wave's bars, as in the app icon's SVG: (x, y, height).
const WAVE: [(f32, f32, f32); 5] = [
    (18.2955, 25.4468, 7.5591),
    (24.2955, 21.6673, 15.1181),
    (30.2955, 17.8878, 22.6772),
    (36.2955, 21.6673, 15.1181),
    (42.2955, 25.4468, 7.5591),
];
/// A bar's width; its ends are round.
const BAR: f32 = 3.7795;

/// The speech bubble's outline, from the app icon's SVG (its path, made
/// absolute): from here, a closed run of cubics.
const BUBBLE_START: (f32, f32) = (20.1771, 57.7297);
/// Each cubic: two control points, then where it ends.
const BUBBLE: [[f32; 6]; 15] = [
    [18.8888, 57.0860, 24.6727, 51.6727, 23.9692, 50.0495],
    [23.2657, 48.4262, 21.0998, 47.9406, 15.5880, 47.9168],
    [11.8340, 47.9006, 8.9352, 49.8457, 4.4712, 46.1577],
    [2.9864, 44.9310, 4.0031, 41.2307, 4.0347, 33.9844],
    [4.0611, 27.9133, 2.4831, 29.8983, 1.9806, 23.6601],
    [1.5029, 17.7305, 2.0651, 16.7071, 5.1609, 11.9286],
    [8.2897, 7.0994, 15.1992, 9.5745, 21.2780, 7.9628],
    [29.4511, 5.7958, 33.1874, 11.2931, 43.3676, 9.5722],
    [51.4764, 8.2015, 53.6873, 7.6206, 57.9272, 9.9456],
    [59.3954, 10.7507, 62.3708, 16.2293, 62.4012, 21.8092],
    [62.4268, 26.4988, 59.9923, 30.5632, 59.6983, 34.7153],
    [59.3877, 39.1007, 62.9104, 41.3290, 59.9965, 44.9506],
    [58.3815, 46.9579, 53.5512, 47.9130, 49.3946, 48.1167],
    [44.0574, 48.3783, 40.7917, 49.9775, 37.6228, 50.3985],
    [34.4540, 50.8195, 21.4653, 58.3734, 20.1771, 57.7297],
];

/// The speech bubble: a round blob with a tail at the lower left.
fn bubble() -> tiny_skia::Path {
    let mut p = PathBuilder::new();
    p.move_to(BUBBLE_START.0, BUBBLE_START.1);
    for [x1, y1, x2, y2, x, y] in BUBBLE {
        p.cubic_to(x1, y1, x2, y2, x, y);
    }
    p.close();
    p.finish().expect("a closed path")
}

/// How far a cubic's control points sit along the tangents to round a
/// quarter circle.
const KAPPA: f32 = 0.552_284_8;

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    Rect::from_xywh(x, y, w, h)?;
    let k = r * KAPPA;
    let (right, bottom) = (x + w, y + h);
    let mut p = PathBuilder::new();
    p.move_to(x + r, y);
    p.line_to(right - r, y);
    p.cubic_to(right - r + k, y, right, y + r - k, right, y + r);
    p.line_to(right, bottom - r);
    p.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    p.line_to(x + r, bottom);
    p.cubic_to(x + r - k, bottom, x, bottom - r + k, x, bottom - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    p.close();
    p.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ARGB pixel at (x, y).
    fn pixel(image: &Image, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * image.width + x) * 4) as usize;
        image.argb[i..i + 4].try_into().unwrap()
    }

    /// The 64 px icon, the largest.
    fn largest(look: Look, accent: Accent) -> &'static Image {
        let image = look.images(accent).last().unwrap();
        assert_eq!(image.width, 64);
        image
    }

    /// Writes each look's 64 px icon as raw ARGB, for a look at them.
    #[test]
    #[ignore = "writes files; run with BLURT_SNAPSHOTS set"]
    fn dump() {
        let Some(dir) = std::env::var_os("BLURT_SNAPSHOTS") else {
            return;
        };
        for look in [
            Look::Disconnected,
            Look::Connected,
            Look::Transmitting,
            Look::Muted,
            Look::Deafened,
        ] {
            for image in [&look.images(Accent::Zune)[0], largest(look, Accent::Zune)] {
                let name = format!("tray-{look:?}-{}.argb", image.width);
                std::fs::write(std::path::Path::new(&dir).join(name), &image.argb).unwrap();
            }
        }
        let app = app_icon(64);
        std::fs::write(std::path::Path::new(&dir).join("app-64.argb"), &app.argb).unwrap();
    }

    #[test]
    fn every_look_at_every_size() {
        for look in [
            Look::Disconnected,
            Look::Connected,
            Look::Transmitting,
            Look::Muted,
            Look::Deafened,
        ] {
            let images = look.images(Accent::Acid);
            assert_eq!(images.len(), SIZES.len());
            for (image, &size) in images.iter().zip(&SIZES) {
                assert_eq!((image.width, image.height), (size, size));
                assert_eq!(image.argb.len(), (size * size * 4) as usize);
            }
        }
    }

    #[test]
    fn app_icon_is_the_logo() {
        let icon = app_icon(64);
        assert_eq!((icon.width, icon.height), (64, 64));
        assert_eq!(pixel(&icon, 1, 1)[0], 0);
        let [a, r, g, _] = pixel(&icon, 10, 40);
        assert!(a == 255 && r > 200 && g < 150, "pink: {:?}", [a, r, g]);
        // The wave is white.
        assert_eq!(pixel(&icon, 32, 29), [255; 4]);
    }

    #[test]
    fn colours_tell_the_state() {
        let at = |look: Look| pixel(largest(look, Accent::Zune), 10, 40);
        // A corner is see-through; the bubble's left edge is solid.
        assert_eq!(pixel(largest(Look::Connected, Accent::Zune), 1, 1)[0], 0);
        let [a, r, g, b] = at(Look::Connected);
        assert_eq!(a, 255);
        assert!(
            r > 200 && g > 200 && b > 200 && r.abs_diff(b) < 20 && g.abs_diff(b) < 20,
            "white: {:?}",
            at(Look::Connected)
        );
        assert_ne!(at(Look::Connected), at(Look::Transmitting));
        let [_, r, g, b] = at(Look::Disconnected);
        assert!(r.abs_diff(b) < 20 && g.abs_diff(b) < 20, "grey");
        // Transmitting: zune's pink.
        let [_, r, g, _] = at(Look::Transmitting);
        assert!(
            r > 200 && g < 150,
            "zune pink: {:?}",
            at(Look::Transmitting)
        );
        // Struck through: red on the diagonal.
        let [_, r, g, _] = pixel(largest(Look::Muted, Accent::Zune), 32, 32);
        assert!(r > 200 && g < 100, "red: {r} {g}");
        assert_eq!(Look::new(true, true, false, true), Look::Deafened);
        assert_eq!(Look::new(true, true, true, false), Look::Muted);
        assert_eq!(Look::new(true, true, false, false), Look::Transmitting);
        assert_eq!(Look::new(false, true, false, false), Look::Disconnected);
    }
}
