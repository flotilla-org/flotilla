//! Status glyphs + colour mapping for convoy / work phases.

use ratatui::style::{Color, Modifier, Style};

use crate::convoy_model::{ConvoyPhase, WorkPhase};

pub struct Glyph {
    pub symbol: &'static str,
    pub style: Style,
}

pub fn convoy_glyph(phase: ConvoyPhase) -> Glyph {
    match phase {
        ConvoyPhase::Pending => Glyph { symbol: "○", style: Style::default().add_modifier(Modifier::DIM) },
        ConvoyPhase::Active => Glyph { symbol: "●", style: Style::default().fg(Color::Green) },
        ConvoyPhase::Completed => Glyph { symbol: "✓", style: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) },
        ConvoyPhase::Failed => Glyph { symbol: "✗", style: Style::default().fg(Color::Red) },
        ConvoyPhase::Cancelled => Glyph { symbol: "⊘", style: Style::default().fg(Color::Red).add_modifier(Modifier::DIM) },
    }
}

pub fn task_glyph(phase: WorkPhase) -> Glyph {
    match phase {
        WorkPhase::Pending => Glyph { symbol: "○", style: Style::default().add_modifier(Modifier::DIM) },
        WorkPhase::Ready => Glyph { symbol: "◐", style: Style::default().fg(Color::Yellow) },
        WorkPhase::Launching => Glyph { symbol: "◑", style: Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) },
        WorkPhase::Running => Glyph { symbol: "●", style: Style::default().fg(Color::Green) },
        WorkPhase::Completed => Glyph { symbol: "✓", style: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) },
        WorkPhase::Failed => Glyph { symbol: "✗", style: Style::default().fg(Color::Red) },
        WorkPhase::Cancelled => Glyph { symbol: "⊘", style: Style::default().fg(Color::Red).add_modifier(Modifier::DIM) },
    }
}
