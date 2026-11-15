//! The push-to-talk pill's mic meter: a row of thin bars that rise with
//! your voice.

use std::time::Instant;

use iced::widget::canvas::{self, Path};
use iced::{Color, Element, Length, Point, Rectangle, Renderer, Size, mouse};

use crate::Message;

const BAR: Size = Size::new(3.0, 22.0);
const GAP: f32 = 3.0;
/// Bars at rest stand this tall, as a fraction of full height.
const REST: f32 = 0.12;

/// `bars` bars showing `level` (linear, as from the engine). `live` lets
/// them move; otherwise they stand at rest. `t` drives the per-bar wobble.
pub fn meter<'a>(
    bars: usize,
    level: f32,
    live: bool,
    color: Color,
    t: Instant,
) -> Element<'a, Message> {
    let width = bars as f32 * BAR.width + (bars.saturating_sub(1)) as f32 * GAP;
    canvas::Canvas::new(Meter {
        heights: heights(bars, if live { level } else { 0.0 }, t),
        color,
    })
    .width(Length::Fixed(width))
    .height(Length::Fixed(BAR.height))
    .into()
}

/// A voice's level as 0 to 1 on a meter: -54 dBFS and below is nothing.
pub fn loudness(level: f32) -> f32 {
    if level <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * level.log10();
    ((db + 54.0) / 48.0).clamp(0.0, 1.0)
}

/// Each bar's height, from [`REST`] to 1. One level, spread over the bars
/// with a fixed shape and a slow per-bar wobble so it reads as a voice.
fn heights(bars: usize, level: f32, t: Instant) -> Vec<f32> {
    let loud = loudness(level);
    // Seconds since an arbitrary start; only differences matter.
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    let secs = t
        .duration_since(*START.get_or_init(Instant::now))
        .as_secs_f32();
    (0..bars)
        .map(|i| {
            let x = i as f32 / (bars.max(2) - 1) as f32;
            // Taller in the middle.
            let shape = 0.55 + 0.45 * (1.0 - (2.0 * x - 1.0).powi(2));
            let rate = 5.0 + 1.7 * i as f32;
            let wobble = 0.75 + 0.25 * (secs * rate + i as f32 * 1.9).sin();
            REST + (1.0 - REST) * (loud * shape * wobble).clamp(0.0, 1.0)
        })
        .collect()
}

struct Meter {
    heights: Vec<f32>,
    color: Color,
}

impl canvas::Program<Message> for Meter {
    type State = ();

    fn draw(
        &self,
        _state: &(),
        renderer: &Renderer,
        _theme: &iced::Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<canvas::Geometry> {
        let mut frame = canvas::Frame::new(renderer, bounds.size());
        for (i, height) in self.heights.iter().enumerate() {
            let h = BAR.height * height;
            let x = i as f32 * (BAR.width + GAP);
            frame.fill(
                &Path::rounded_rectangle(
                    Point::new(x, BAR.height - h),
                    Size::new(BAR.width, h),
                    1.5.into(),
                ),
                self.color,
            );
        }
        vec![frame.into_geometry()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_is_at_rest_and_loud_is_tall() {
        let now = Instant::now();
        assert!(heights(12, 0.0, now).iter().all(|&h| h == REST));
        let loud = heights(12, 0.5, now);
        assert!(loud.iter().all(|&h| h > REST && h <= 1.0));
        assert!(loud.iter().cloned().fold(0.0, f32::max) > 0.6);
    }

    #[test]
    fn loudness_maps_dbfs() {
        assert_eq!(loudness(0.0), 0.0);
        assert_eq!(loudness(0.001), 0.0); // -60 dBFS
        assert!((loudness(0.5) - 0.995).abs() < 0.01); // about -6 dBFS
    }
}
