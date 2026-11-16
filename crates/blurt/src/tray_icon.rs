//! The tray icon, drawn here rather than shipped as images: blurt's app
//! icon (packaging/icons), a speech bubble with a sound wave, in a few
//! states. Connected, it's the app icon's blue; otherwise grey. While
//! you're being heard, it lights up in the accent, which in blurt only
//! ever means live audio. Muted or deafened, it's struck through: a muted
//! wave in a blue bubble, or (deaf) a grey one.

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
        IMAGES[self as usize][a]
            .get_or_init(|| SIZES.iter().map(|&size| draw(self, accent, size)).collect())
    }
}

// Windows asks for its small icon size times the display scale (125%:
// 20, 150%: 24, ...): drawn at each, it never scales one itself.
#[cfg(windows)]
const SIZES: [u32; 7] = [16, 20, 24, 32, 40, 48, 64];
#[cfg(not(windows))]
const SIZES: [u32; 4] = [16, 22, 32, 64];

/// The app icon's design grid.
const GRID: f32 = 128.0;

fn rgb(hex: u32) -> Color {
    Color::from_rgba8((hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 255)
}

fn from_iced(c: iced::Color) -> Color {
    Color::from_rgba(c.r, c.g, c.b, c.a).expect("a valid colour")
}

fn gradient(from: Color, to: Color) -> Shader<'static> {
    LinearGradient::new(
        Point::from_xy(0.0, 0.0),
        Point::from_xy(GRID, GRID),
        vec![GradientStop::new(0.0, from), GradientStop::new(1.0, to)],
        SpreadMode::Pad,
        Transform::identity(),
    )
    .expect("a valid gradient")
}

fn draw(look: Look, accent: Accent, size: u32) -> Image {
    let mut pixmap = Pixmap::new(size, size).expect("a non-empty icon");
    let scale = size as f32 / GRID;
    let transform = Transform::from_scale(scale, scale);

    // The bubble.
    let mut paint = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    paint.shader = match look {
        Look::Transmitting => {
            let (a1, a2) = accent.ends();
            gradient(from_iced(a1), from_iced(a2))
        }
        Look::Connected | Look::Muted => gradient(rgb(0x5b8def), rgb(0x3a5bc7)),
        Look::Disconnected | Look::Deafened => Shader::SolidColor(rgb(0x7c828d)),
    };
    pixmap.fill_path(&bubble(), &paint, FillRule::Winding, transform, None);

    // The wave: dark on the accent, as text is in the app; dimmed while it
    // isn't going anywhere.
    let mut white = Paint {
        anti_alias: true,
        ..Paint::default()
    };
    let struck = matches!(look, Look::Muted | Look::Deafened);
    white.set_color(if look == Look::Transmitting {
        from_iced(crate::theme::ON_ACCENT)
    } else {
        Color::from_rgba(1.0, 1.0, 1.0, if struck { 0.45 } else { 1.0 }).unwrap()
    });
    for (x, y, h) in [
        (31., 47., 18.),
        (45., 37., 38.),
        (59., 29., 54.),
        (73., 39., 34.),
        (87., 48., 16.),
    ] {
        if let Some(bar) = rounded_rect(x, y, 8.0, h, 4.0) {
            pixmap.fill_path(&bar, &white, FillRule::Winding, transform, None);
        }
    }

    // The strike: red, edged in white so it reads on any panel.
    if struck {
        let mut line = PathBuilder::new();
        line.move_to(22.0, 22.0);
        line.line_to(106.0, 106.0);
        let line = line.finish().expect("a line");
        let mut edge = Paint {
            anti_alias: true,
            ..Paint::default()
        };
        edge.set_color(rgb(0xffffff));
        let stroke = |width| Stroke {
            width,
            line_cap: tiny_skia::LineCap::Round,
            ..Stroke::default()
        };
        pixmap.stroke_path(&line, &edge, &stroke(22.0), transform, None);
        let mut red = edge.clone();
        red.set_color(rgb(0xe5484d));
        pixmap.stroke_path(&line, &red, &stroke(12.0), transform, None);
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

/// How far a cubic's control points sit along the tangents to round a
/// quarter circle.
const KAPPA: f32 = 0.552_284_8;

/// The speech bubble: a rounded rectangle with a tail at the lower left,
/// as in the app icon's SVG.
fn bubble() -> tiny_skia::Path {
    let (left, top, right, bottom, r) = (12.0, 16.0, 116.0, 92.0, 12.0);
    let k = r * KAPPA;
    let mut p = PathBuilder::new();
    p.move_to(left + r, top);
    p.line_to(right - r, top);
    p.cubic_to(right - r + k, top, right, top + r - k, right, top + r);
    p.line_to(right, bottom - r);
    p.cubic_to(
        right,
        bottom - r + k,
        right - r + k,
        bottom,
        right - r,
        bottom,
    );
    // The tail.
    p.line_to(58.0, bottom);
    p.line_to(36.0, 112.0);
    p.line_to(36.0, bottom);
    p.line_to(left + r, bottom);
    p.cubic_to(left + r - k, bottom, left, bottom - r + k, left, bottom - r);
    p.line_to(left, top + r);
    p.cubic_to(left, top + r - k, left + r - k, top, left + r, top);
    p.close();
    p.finish().expect("a closed path")
}

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
            let image = largest(look, Accent::Zune);
            let path = std::path::Path::new(&dir).join(format!("tray-{look:?}.argb"));
            std::fs::write(path, &image.argb).unwrap();
        }
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
    fn colours_tell_the_state() {
        let at = |look: Look| pixel(largest(look, Accent::Zune), 10, 40);
        // A corner is see-through; the bubble's left edge is solid.
        assert_eq!(pixel(largest(Look::Connected, Accent::Zune), 1, 1)[0], 0);
        let [a, r, _, b] = at(Look::Connected);
        assert_eq!(a, 255);
        assert!(b > r + 60, "blue: {:?}", at(Look::Connected));
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
