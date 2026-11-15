//! Line icons, drawn as vector strokes rather than from an icon font.
//!
//! The shapes are Lucide's (https://lucide.dev, ISC licence; see
//! `assets/LICENSE-lucide`), as SVG path data on Lucide's 24 × 24 grid.
//! `rect`, `line` and `polyline` elements are written out as paths.

use std::cell::Cell;
use std::sync::OnceLock;

use iced::mouse;
use iced::widget::canvas::path::lyon_path::geom::{self as lyon, euclid};
use iced::widget::canvas::{self, LineCap, LineJoin, Path, Stroke};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Vector};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Glyph {
    Mic,
    MicOff,
    Headphones,
    HeadphonesOff,
    Lock,
    Send,
    Leave,
    Volume,
    Minimize,
    Maximize,
    Close,
}

impl Glyph {
    const ALL: [Glyph; 11] = [
        Glyph::Mic,
        Glyph::MicOff,
        Glyph::Headphones,
        Glyph::HeadphonesOff,
        Glyph::Lock,
        Glyph::Send,
        Glyph::Leave,
        Glyph::Volume,
        Glyph::Minimize,
        Glyph::Maximize,
        Glyph::Close,
    ];

    fn source(self) -> &'static [&'static str] {
        match self {
            Glyph::Mic => &[
                "M12 2a3 3 0 0 0-3 3v7a3 3 0 0 0 6 0V5a3 3 0 0 0-3-3Z",
                "M19 10v2a7 7 0 0 1-14 0v-2",
                "M12 19V22",
            ],
            Glyph::MicOff => &[
                "M2 2L22 22",
                "M18.89 13.23A7.12 7.12 0 0 0 19 12v-2",
                "M5 10v2a7 7 0 0 0 12 5",
                "M15 9.34V5a3 3 0 0 0-5.68-1.33",
                "M9 9v3a3 3 0 0 0 5.12 2.12",
                "M12 19V22",
            ],
            Glyph::Headphones => &[
                "M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 18 0v7a2 2 0 0 1-2 2h-1a2 2 0 0 1-2-2v-3a2 2 0 0 1 2-2h3",
            ],
            Glyph::HeadphonesOff => &[
                "M21 14h-1.343",
                "M9.128 3.47A9 9 0 0 1 21 12v3.343",
                "m2 2 20 20",
                "M20.414 20.414A2 2 0 0 1 19 21h-1a2 2 0 0 1-2-2v-3",
                "M3 14h3a2 2 0 0 1 2 2v3a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a9 9 0 0 1 2.636-6.364",
            ],
            Glyph::Lock => &[
                "M5 11h14a2 2 0 0 1 2 2v7a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-7a2 2 0 0 1 2-2Z",
                "M7 11V7a5 5 0 0 1 10 0v4",
            ],
            Glyph::Send => &["m5 12 7-7 7 7", "M12 19V5"],
            Glyph::Leave => &[
                "M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4",
                "M16 17L21 12L16 7",
                "M21 12H9",
            ],
            Glyph::Volume => &[
                "M11 4.702a.705.705 0 0 0-1.203-.498L6.413 7.587A1.4 1.4 0 0 1 5.416 8H3a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h2.416a1.4 1.4 0 0 1 .997.413l3.383 3.384A.705.705 0 0 0 11 19.298z",
                "M16 9a5 5 0 0 1 0 6",
                "M19.364 18.364a9 9 0 0 0 0-12.728",
            ],
            Glyph::Minimize => &["M5 12h14"],
            Glyph::Maximize => {
                &["M5 3h14a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2Z"]
            }
            Glyph::Close => &["M18 6 6 18", "m6 6 12 12"],
        }
    }

    /// The glyph on the 24 × 24 grid, parsed once.
    fn path(self) -> &'static Path {
        static PATHS: OnceLock<Vec<Path>> = OnceLock::new();
        let paths = PATHS.get_or_init(|| {
            Glyph::ALL
                .iter()
                .map(|glyph| {
                    Path::new(|b| {
                        for d in glyph.source() {
                            if let Err(e) = svg_path(b, d) {
                                log::error!("Bad icon path for {glyph:?}: {e}");
                            }
                        }
                    })
                })
                .collect()
        });
        let index = Glyph::ALL.iter().position(|g| *g == self).expect("in ALL");
        &paths[index]
    }
}

/// An icon `size` px square, stroked in `color`. `stroke` is in px at that
/// size (Lucide's own is 2 on 24, i.e. `size / 12`).
pub fn icon<'a, Message: 'a>(
    glyph: Glyph,
    size: f32,
    stroke: f32,
    color: Color,
) -> Element<'a, Message> {
    canvas::Canvas::new(Icon {
        glyph,
        stroke,
        color,
    })
    .width(Length::Fixed(size))
    .height(Length::Fixed(size))
    .into()
}

/// Lucide's own weight at `size`.
pub fn standard<'a, Message: 'a>(glyph: Glyph, size: f32, color: Color) -> Element<'a, Message> {
    icon(glyph, size, (size / 12.0).max(1.25), color)
}

struct Icon {
    glyph: Glyph,
    stroke: f32,
    color: Color,
}

/// The drawing, kept until the icon's glyph, colour or stroke changes.
#[derive(Default)]
struct IconState {
    cache: canvas::Cache,
    drawn: Cell<Option<(Glyph, Color, f32)>>,
}

impl<Message> canvas::Program<Message> for Icon {
    type State = IconState;

    fn draw(
        &self,
        state: &IconState,
        renderer: &Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let key = Some((self.glyph, self.color, self.stroke));
        if state.drawn.get() != key {
            state.cache.clear();
            state.drawn.set(key);
        }
        let geometry = state.cache.draw(renderer, bounds.size(), |frame| {
            let scale = bounds.width.min(bounds.height) / 24.0;
            frame.scale(scale);
            frame.stroke(
                self.glyph.path(),
                Stroke::default()
                    .with_color(self.color)
                    .with_width(self.stroke / scale)
                    .with_line_cap(LineCap::Round)
                    .with_line_join(LineJoin::Round),
            );
        });
        vec![geometry]
    }
}

/// Appends SVG path data to `b`. Supports the commands Lucide uses: M, L,
/// H, V, A, C, Q, Z, absolute and relative.
fn svg_path(b: &mut canvas::path::Builder, d: &str) -> Result<(), String> {
    let mut tokens = Tokens::new(d);
    let mut current = Point::ORIGIN;
    let mut start = Point::ORIGIN;
    let mut command = None;
    while let Some(token) = tokens.peek() {
        let c = match token {
            Token::Command(c) => {
                tokens.next();
                command = Some(c);
                c
            }
            // Repeated arguments repeat the command (M repeats as L).
            Token::Number(_) => match command {
                Some('M') => 'L',
                Some('m') => 'l',
                Some(c) => c,
                None => return Err("number before any command".into()),
            },
        };
        let relative = c.is_ascii_lowercase();
        let origin = if relative {
            Vector::new(current.x, current.y)
        } else {
            Vector::ZERO
        };
        let point = |tokens: &mut Tokens| -> Result<Point, String> {
            let x = tokens.number()?;
            let y = tokens.number()?;
            Ok(Point::new(x, y) + origin)
        };
        match c.to_ascii_uppercase() {
            'M' => {
                current = point(&mut tokens)?;
                start = current;
                b.move_to(current);
                // Following pairs are line-tos.
                command = Some(if relative { 'l' } else { 'L' });
            }
            'L' => {
                current = point(&mut tokens)?;
                b.line_to(current);
            }
            'H' => {
                let x = tokens.number()?;
                current.x = if relative { current.x + x } else { x };
                b.line_to(current);
            }
            'V' => {
                let y = tokens.number()?;
                current.y = if relative { current.y + y } else { y };
                b.line_to(current);
            }
            'C' => {
                let c1 = point(&mut tokens)?;
                let c2 = point(&mut tokens)?;
                let to = point(&mut tokens)?;
                b.bezier_curve_to(c1, c2, to);
                current = to;
            }
            'Q' => {
                let c1 = point(&mut tokens)?;
                let to = point(&mut tokens)?;
                b.quadratic_curve_to(c1, to);
                current = to;
            }
            'A' => {
                let rx = tokens.number()?;
                let ry = tokens.number()?;
                let rotation = tokens.number()?;
                let large_arc = tokens.flag()?;
                let sweep = tokens.flag()?;
                let to = point(&mut tokens)?;
                let arc = lyon::SvgArc {
                    from: euclid::point2(current.x, current.y),
                    to: euclid::point2(to.x, to.y),
                    radii: euclid::vec2(rx, ry),
                    x_rotation: lyon::Angle::degrees(rotation),
                    flags: lyon::ArcFlags { large_arc, sweep },
                };
                if arc.is_straight_line() {
                    b.line_to(to);
                } else {
                    arc.for_each_quadratic_bezier(&mut |q| {
                        b.quadratic_curve_to(
                            Point::new(q.ctrl.x, q.ctrl.y),
                            Point::new(q.to.x, q.to.y),
                        );
                    });
                }
                current = to;
            }
            'Z' => {
                b.close();
                current = start;
            }
            other => return Err(format!("unsupported command {other}")),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token {
    Command(char),
    Number(f32),
}

struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Tokens<'a> {
    fn new(d: &'a str) -> Self {
        Self { rest: d }
    }

    fn skip_separators(&mut self) {
        self.rest = self
            .rest
            .trim_start_matches(|c: char| c.is_whitespace() || c == ',');
    }

    fn peek(&mut self) -> Option<Token> {
        self.skip_separators();
        let c = self.rest.chars().next()?;
        if c.is_ascii_alphabetic() {
            Some(Token::Command(c))
        } else {
            Some(Token::Number(0.0))
        }
    }

    fn next(&mut self) {
        self.skip_separators();
        let mut chars = self.rest.chars();
        chars.next();
        self.rest = chars.as_str();
    }

    /// A number, which may run straight into the next: `.705.705`, `0-1.2`.
    fn number(&mut self) -> Result<f32, String> {
        self.skip_separators();
        let bytes = self.rest.as_bytes();
        let mut end = 0;
        if end < bytes.len() && (bytes[end] == b'-' || bytes[end] == b'+') {
            end += 1;
        }
        let mut seen_dot = false;
        while end < bytes.len() {
            match bytes[end] {
                b'0'..=b'9' => end += 1,
                b'.' if !seen_dot => {
                    seen_dot = true;
                    end += 1;
                }
                _ => break,
            }
        }
        let (number, rest) = self.rest.split_at(end);
        self.rest = rest;
        number
            .parse()
            .map_err(|_| format!("expected a number at {:?}", number.to_owned() + rest))
    }

    /// Arc flags are single digits and may be written without separators.
    fn flag(&mut self) -> Result<bool, String> {
        self.skip_separators();
        let flag = match self.rest.as_bytes().first() {
            Some(b'0') => false,
            Some(b'1') => true,
            _ => return Err(format!("expected an arc flag at {:?}", self.rest)),
        };
        self.rest = &self.rest[1..];
        Ok(flag)
    }
}

/// For tests: the bounds of a glyph's path on the 24 × 24 grid.
#[cfg(test)]
fn bounds(glyph: Glyph) -> (Point, iced::Size) {
    use iced::widget::canvas::path::lyon_path::iterator::PathIterator;
    let mut min = Point::new(f32::MAX, f32::MAX);
    let mut max = Point::new(f32::MIN, f32::MIN);
    for event in glyph.path().raw().iter().flattened(0.1) {
        if let iced::widget::canvas::path::lyon_path::Event::Line { from, to } = event {
            for p in [from, to] {
                min = Point::new(min.x.min(p.x), min.y.min(p.y));
                max = Point::new(max.x.max(p.x), max.y.max(p.y));
            }
        }
    }
    (min, iced::Size::new(max.x - min.x, max.y - min.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_split_where_svg_says() {
        let mut t = Tokens::new(".705.705 0 0 0-1.203-.498");
        let numbers: Vec<f32> = (0..6).map(|_| t.number().unwrap()).collect();
        assert_eq!(numbers, [0.705, 0.705, 0.0, 0.0, 0.0, -1.203]);
        assert_eq!(t.number().unwrap(), -0.498);
    }

    #[test]
    fn every_glyph_parses_and_fits_the_grid() {
        for glyph in Glyph::ALL {
            let mut ok = true;
            Path::new(|b| {
                for d in glyph.source() {
                    ok &= svg_path(b, d).is_ok();
                }
            });
            assert!(ok, "{glyph:?}");
            let (origin, size) = bounds(glyph);
            assert!(
                origin.x >= 1.0 && origin.y >= 1.0,
                "{glyph:?} at {origin:?}"
            );
            assert!(origin.x + size.width <= 23.0, "{glyph:?} {size:?}");
            assert!(origin.y + size.height <= 23.0, "{glyph:?} {size:?}");
            assert!(size.width > 4.0 || size.height > 4.0, "{glyph:?} is empty");
        }
    }

    #[test]
    fn mic_spans_its_svg_bounds() {
        // The capsule runs 9..15 across and the stand down to y = 22.
        let (origin, size) = bounds(Glyph::Mic);
        assert!((origin.x - 5.0).abs() < 0.1, "{origin:?}");
        assert!((origin.y - 2.0).abs() < 0.1, "{origin:?}");
        assert!((origin.y + size.height - 22.0).abs() < 0.1, "{size:?}");
    }
}
