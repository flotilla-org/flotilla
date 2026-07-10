//! Status glyphs + colour mapping for convoy / leg phases.

use ratatui::style::{Color, Modifier, Style};

use crate::convoy_model::{ConvoyPhase, TaskPhase};

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

pub fn task_glyph(phase: TaskPhase) -> Glyph {
    match phase {
        TaskPhase::Pending => Glyph { symbol: "○", style: Style::default().add_modifier(Modifier::DIM) },
        TaskPhase::Ready => Glyph { symbol: "◐", style: Style::default().fg(Color::Yellow) },
        TaskPhase::Launching => Glyph { symbol: "◑", style: Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD) },
        TaskPhase::Running => Glyph { symbol: "●", style: Style::default().fg(Color::Green) },
        TaskPhase::Completed => Glyph { symbol: "✓", style: Style::default().fg(Color::Green).add_modifier(Modifier::BOLD) },
        TaskPhase::Failed => Glyph { symbol: "✗", style: Style::default().fg(Color::Red) },
        TaskPhase::Cancelled => Glyph { symbol: "⊘", style: Style::default().fg(Color::Red).add_modifier(Modifier::DIM) },
    }
}
