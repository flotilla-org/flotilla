use std::{
    sync::OnceLock,
    time::{Duration, Instant},
};

use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};

static PROCESS_START: OnceLock<Instant> = OnceLock::new();

fn elapsed_since_start() -> Duration {
    PROCESS_START.get_or_init(Instant::now).elapsed()
}

fn has_true_color() -> bool {
    static TRUE_COLOR: OnceLock<bool> = OnceLock::new();
    *TRUE_COLOR.get_or_init(|| std::env::var("COLORTERM").map(|v| v == "truecolor" || v == "24bit").unwrap_or(false))
}

fn blend(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let r = (a.0 as f32 * t + b.0 as f32 * (1.0 - t)) as u8;
    let g = (a.1 as f32 * t + b.1 as f32 * (1.0 - t)) as u8;
    let b_val = (a.2 as f32 * t + b.2 as f32 * (1.0 - t)) as u8;
    (r, g, b_val)
}

/// Creates a shimmer animation effect: a bright band sweeps across the text
/// on a 2-second cycle, blending between dim and bright yellow.
pub(crate) fn shimmer_spans(text: &str) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }

    let padding = 10usize;
    let period = chars.len() + padding * 2;
    let sweep_seconds = 2.0f32;
    let pos = (elapsed_since_start().as_secs_f32() % sweep_seconds) / sweep_seconds * period as f32;
    let band_half_width = 5.0f32;

    let true_color = has_true_color();
    let base: (u8, u8, u8) = (140, 130, 40);
    let highlight: (u8, u8, u8) = (255, 240, 120);

    let mut spans = Vec::with_capacity(chars.len());
    for (i, ch) in chars.iter().enumerate() {
        let dist = ((i as f32 + padding as f32) - pos).abs();
        let t = if dist <= band_half_width { 0.5 * (1.0 + (std::f32::consts::PI * dist / band_half_width).cos()) } else { 0.0 };

        let style = if true_color {
            let (r, g, b) = blend(highlight, base, t);
            Style::default().fg(Color::Rgb(r, g, b))
        } else if t < 0.2 {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::DIM)
        } else if t < 0.6 {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
        };

        spans.push(Span::styled(ch.to_string(), style));
    }
    spans
}
