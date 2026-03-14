//! Showcase of interesting glyphs available in terminal UIs.
//!
//! Run with: cargo run --example glyph_showcase

use std::io;

use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEventKind},
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap},
    Frame,
};

struct App {
    scroll: u16,
    sections: Vec<GlyphSection>,
}

struct GlyphSection {
    title: &'static str,
    color: Color,
    rows: Vec<GlyphRow>,
}

struct GlyphRow {
    label: &'static str,
    glyphs: &'static str,
}

fn build_sections() -> Vec<GlyphSection> {
    vec![
        GlyphSection {
            title: "Box Drawing — Light",
            color: Color::Cyan,
            rows: vec![
                GlyphRow { label: "Horizontal/Vertical", glyphs: "─ │ ┌ ┐ └ ┘ ├ ┤ ┬ ┴ ┼" },
                GlyphRow { label: "Rounded corners", glyphs: "╭ ╮ ╰ ╯" },
                GlyphRow { label: "Dashed", glyphs: "┄ ┆ ┈ ┊ ╌ ╎" },
                GlyphRow { label: "Example box", glyphs: "╭───────╮\n│ hello │\n╰───────╯" },
            ],
        },
        GlyphSection {
            title: "Box Drawing — Heavy & Double",
            color: Color::Blue,
            rows: vec![
                GlyphRow { label: "Heavy", glyphs: "━ ┃ ┏ ┓ ┗ ┛ ┣ ┫ ┳ ┻ ╋" },
                GlyphRow { label: "Double", glyphs: "═ ║ ╔ ╗ ╚ ╝ ╠ ╣ ╦ ╩ ╬" },
                GlyphRow { label: "Mixed light/heavy", glyphs: "┍ ┑ ┕ ┙ ┝ ┥ ┯ ┷ ┿ ╀ ╁ ╂" },
                GlyphRow { label: "Example double box", glyphs: "╔═══════╗\n║ hello ║\n╚═══════╝" },
            ],
        },
        GlyphSection {
            title: "Block Elements",
            color: Color::Green,
            rows: vec![
                GlyphRow { label: "Full & half", glyphs: "█ ▉ ▊ ▋ ▌ ▍ ▎ ▏ ▐" },
                GlyphRow { label: "Vertical halves", glyphs: "▀ ▄ ▔ ▁ ▂ ▃ ▅ ▆ ▇" },
                GlyphRow { label: "Shading", glyphs: "░ ▒ ▓ █" },
                GlyphRow { label: "Quadrants", glyphs: "▖ ▗ ▘ ▙ ▚ ▛ ▜ ▝ ▞ ▟" },
                GlyphRow {
                    label: "Bar chart",
                    glyphs: "▁▂▃▄▅▆▇█▇▆▅▄▃▂▁  (sparkline)",
                },
                GlyphRow {
                    label: "Horizontal bar",
                    glyphs: "▏▎▍▌▋▊▉█  (progress)",
                },
            ],
        },
        GlyphSection {
            title: "Braille Patterns (2×4 dot grid per char)",
            color: Color::Magenta,
            rows: vec![
                GlyphRow { label: "Dots", glyphs: "⠁ ⠂ ⠄ ⠈ ⠐ ⠠ ⡀ ⢀" },
                GlyphRow { label: "Columns", glyphs: "⡇ ⣿ ⠿ ⠛ ⠉ ⠒ ⠤ ⣤ ⣶ ⣷" },
                GlyphRow { label: "Line drawing", glyphs: "⠑ ⠊ ⠢ ⠔ ⡠ ⢄ ⡰ ⢆ ⡴ ⢎ ⡸ ⢇" },
                GlyphRow {
                    label: "Density ramp",
                    glyphs: "⠀⠁⠃⠇⡇⡏⡟⡿⣿  (empty → full)",
                },
                GlyphRow {
                    label: "Wave pattern",
                    glyphs: "⢀⣀⣄⣤⣴⣶⣾⣿⣷⣶⣴⣤⣄⣀⢀",
                },
            ],
        },
        GlyphSection {
            title: "Arrows & Pointers",
            color: Color::Yellow,
            rows: vec![
                GlyphRow { label: "Simple", glyphs: "← → ↑ ↓ ↔ ↕" },
                GlyphRow { label: "Double", glyphs: "⇐ ⇒ ⇑ ⇓ ⇔ ⇕" },
                GlyphRow { label: "Diagonal", glyphs: "↖ ↗ ↘ ↙ ⬁ ⬀ ⬂ ⬃" },
                GlyphRow { label: "Triangle", glyphs: "◀ ▶ ▲ ▼ ◁ ▷ △ ▽" },
                GlyphRow { label: "Fancy", glyphs: "➜ ➤ ➔ ➙ ➛ ➝ ➞ ➟ ➠ ⏎ ↩ ↪" },
                GlyphRow { label: "Pointing", glyphs: "☛ ☞ ◉ ⊳ ⊲ ≫ ≪" },
            ],
        },
        GlyphSection {
            title: "Geometric Shapes",
            color: Color::Red,
            rows: vec![
                GlyphRow { label: "Squares", glyphs: "■ □ ▪ ▫ ◾ ◽ ⬛ ⬜" },
                GlyphRow { label: "Circles", glyphs: "● ○ ◉ ◎ ⊙ ⊚ ⦿ ⬤" },
                GlyphRow { label: "Diamonds", glyphs: "◆ ◇ ❖ ⬥ ⬦" },
                GlyphRow { label: "Triangles", glyphs: "▲ △ ▴ ▵ ▶ ▷ ▸ ▹ ▼ ▽ ▾ ▿ ◀ ◁ ◂ ◃" },
                GlyphRow { label: "Stars", glyphs: "★ ☆ ✦ ✧ ✩ ✪ ✫ ✬ ✭ ✮ ✯ ✰ ⍟" },
                GlyphRow { label: "Misc", glyphs: "⬡ ⬢ ⏣ ⎔ ⌬" },
            ],
        },
        GlyphSection {
            title: "Mathematical & Logical",
            color: Color::Cyan,
            rows: vec![
                GlyphRow { label: "Operators", glyphs: "± × ÷ ∓ ∗ ∘ √ ∛ ∜" },
                GlyphRow { label: "Comparison", glyphs: "≈ ≠ ≤ ≥ ≡ ≢ ≪ ≫ ≲ ≳" },
                GlyphRow { label: "Logic", glyphs: "∧ ∨ ¬ ⊕ ⊗ ⊢ ⊣ ⊤ ⊥ ∀ ∃ ∄" },
                GlyphRow { label: "Sets", glyphs: "∈ ∉ ⊂ ⊃ ⊆ ⊇ ∪ ∩ ∅ ℘" },
                GlyphRow { label: "Calculus", glyphs: "∂ ∇ ∫ ∬ ∭ ∮ ∯ ∰ ∞ ∑ ∏" },
                GlyphRow { label: "Greek", glyphs: "α β γ δ ε ζ η θ λ μ π σ φ ψ ω Δ Σ Ω" },
            ],
        },
        GlyphSection {
            title: "Status & UI Indicators",
            color: Color::Green,
            rows: vec![
                GlyphRow { label: "Checks", glyphs: "✓ ✔ ✗ ✘ ☑ ☒ ☐" },
                GlyphRow { label: "Dots / bullets", glyphs: "• ◦ ‣ ⁃ ∙ ⋅ ⦁ ⦂" },
                GlyphRow { label: "Info", glyphs: "ℹ ⓘ ⚠ ⛔ ⚡ ♻ ⟳ ↻ ⏳ ⌛" },
                GlyphRow { label: "Spinners", glyphs: "◐ ◓ ◑ ◒   ⠋ ⠙ ⠹ ⠸ ⠼ ⠴ ⠦ ⠧ ⠇ ⠏" },
                GlyphRow { label: "Progress dots", glyphs: "⣾ ⣽ ⣻ ⢿ ⡿ ⣟ ⣯ ⣷" },
                GlyphRow { label: "Radio buttons", glyphs: "◉ ○  (selected / unselected)" },
                GlyphRow { label: "Scrollbar", glyphs: "▲ █ ░ ▼  │ ┃  ◄ ► ─ ━" },
            ],
        },
        GlyphSection {
            title: "Line Styles (for graphs/charts)",
            color: Color::Blue,
            rows: vec![
                GlyphRow { label: "Thin line set", glyphs: "• ─ │ ┌ ┐ └ ┘" },
                GlyphRow { label: "Thick line set", glyphs: "• ━ ┃ ┏ ┓ ┗ ┛" },
                GlyphRow { label: "Double line set", glyphs: "• ═ ║ ╔ ╗ ╚ ╝" },
                GlyphRow {
                    label: "Axis markers",
                    glyphs: "╶ ╴ ╵ ╷  (half-lines for axis ticks)",
                },
                GlyphRow {
                    label: "Line chart",
                    glyphs: "    ▁▂▃▄▅▆▇█\n 8 ┤      ╭──╮\n 4 ┤  ╭───╯  │\n 0 ┼──╯      ╰──",
                },
                GlyphRow {
                    label: "Bar chart (horiz)",
                    glyphs: "CPU  ████████████████░░░░  78%\nMem  ██████████░░░░░░░░░░  52%\nDisk ██████████████████░░  91%\nNet  ███░░░░░░░░░░░░░░░░░  14%",
                },
                GlyphRow {
                    label: "Bar chart (vert)",
                    glyphs: "     █\n     █  █\n  █  █  █\n  █  █  █  █\n  █  █  █  █  ▄\n ─────────────\n  M  T  W  T  F",
                },
                GlyphRow {
                    label: "Sparkline",
                    glyphs: "Requests: ▂▃▅▇█▇▅▃▂▁▂▄▆█▇▅▃▁▁▂▃▅▇",
                },
                GlyphRow {
                    label: "Braille chart",
                    glyphs: "⠀⠀⠀⠀⠀⠀⣀⡀\n⠀⠀⠀⢀⡠⠊⠀⠈⠢⡀\n⠀⢀⠔⠁⠀⠀⠀⠀⠀⠈⠢⡀\n⠔⠁⠀⠀⠀⠀⠀⠀⠀⠀⠀⠈⠢",
                },
                GlyphRow {
                    label: "Gauge/meter",
                    glyphs: "╶──────────┼──────────╴\n          ▲ 50%\n\n[░░░░░▒▒▒▓▓███████░░░░]  CPU temp",
                },
                GlyphRow {
                    label: "Dot plot",
                    glyphs: "8 ┤            ●\n6 ┤    ●    ●\n4 ┤  ●   ●    ●  ●\n2 ┤●               ●\n0 ┼──┬──┬──┬──┬──┬──┬",
                },
                GlyphRow {
                    label: "Heatmap cells",
                    glyphs: "░░░▒▒▓██▓▒░░░\n░▒▒▓██████▓▒░\n▒▓████████▓▒░\n░▒▒▓██████▓▒░\n░░░▒▒▓██▓▒░░░",
                },
            ],
        },
        GlyphSection {
            title: "Powerline & Nerd Font Separators",
            color: Color::Magenta,
            rows: vec![
                GlyphRow { label: "Powerline", glyphs: "\u{E0B0} \u{E0B1} \u{E0B2} \u{E0B3} \u{E0B4} \u{E0B5} \u{E0B6} \u{E0B7}" },
                GlyphRow { label: "Rounded", glyphs: "\u{E0B4} \u{E0B5} \u{E0B6} \u{E0B7}" },
                GlyphRow { label: "Branch/line/lock", glyphs: "\u{E0A0} \u{E0A1} \u{E0A2}" },
                GlyphRow { label: "File icons", glyphs: "\u{F015} \u{F07B} \u{F07C} \u{F1C0} \u{F121} \u{F1C9} \u{F013} \u{F085}" },
                GlyphRow { label: "Dev icons", glyphs: "\u{E7A8} \u{E706} \u{E718} \u{E796} \u{E60B} \u{F308} \u{E7A1}" },
                GlyphRow { label: "Status icons", glyphs: "\u{F00C} \u{F00D} \u{F071} \u{F05A} \u{F188} \u{F023} \u{F09C}" },
                GlyphRow {
                    label: "Note",
                    glyphs: "Requires a Nerd Font. Boxes/? = font doesn't have these.",
                },
            ],
        },
        GlyphSection {
            title: "Unicode Separators (no Nerd Font needed)",
            color: Color::Magenta,
            rows: vec![
                GlyphRow { label: "Triangles solid", glyphs: "◀ ▶ ◣ ◢ ◤ ◥ ▲ ▼" },
                GlyphRow { label: "Triangles outline", glyphs: "◁ ▷ △ ▽ ◃ ▹" },
                GlyphRow { label: "Half blocks", glyphs: "▌ ▐ ▀ ▄" },
                GlyphRow { label: "Wedges/angles", glyphs: "❮ ❯ ❰ ❱ ⟨ ⟩ ⟪ ⟫ « »" },
                GlyphRow { label: "Slashes", glyphs: "╱ ╲ ╳ ⧸ ⧹" },
                GlyphRow {
                    label: "Status bar example",
                    glyphs: "▌main ▶ src/app.rs ▶ fn render() ▐",
                },
                GlyphRow {
                    label: "Alt status bar",
                    glyphs: "❮ main ❯ src/app.rs ❯ fn render() ❯",
                },
                GlyphRow {
                    label: "Block separator",
                    glyphs: "█▌ Normal █▌ Insert █▌ Visual █▌",
                },
            ],
        },
        GlyphSection {
            title: "Music, Cards & Miscellaneous",
            color: Color::Yellow,
            rows: vec![
                GlyphRow { label: "Music", glyphs: "♩ ♪ ♫ ♬ ♭ ♮ ♯" },
                GlyphRow { label: "Cards", glyphs: "♠ ♣ ♥ ♦ ♤ ♧ ♡ ♢" },
                GlyphRow { label: "Dice", glyphs: "⚀ ⚁ ⚂ ⚃ ⚄ ⚅" },
                GlyphRow { label: "Chess", glyphs: "♔ ♕ ♖ ♗ ♘ ♙ ♚ ♛ ♜ ♝ ♞ ♟" },
                GlyphRow { label: "Weather", glyphs: "☀ ☁ ☂ ☃ ⛅ ⛈ ❄ ❅ ❆" },
                GlyphRow { label: "Zodiac", glyphs: "♈ ♉ ♊ ♋ ♌ ♍ ♎ ♏ ♐ ♑ ♒ ♓" },
                GlyphRow { label: "Misc symbols", glyphs: "☮ ☯ ☠ ☢ ☣ ⚛ ⚙ ⚔ ⚖ ⚗ ⚘ ⚜" },
            ],
        },
        GlyphSection {
            title: "Currency & Legal",
            color: Color::Red,
            rows: vec![
                GlyphRow { label: "Currency", glyphs: "$ € £ ¥ ₹ ₽ ₿ ¢ ₩ ₫ ₺ ₴ ₸ ₡ ₲ ₵" },
                GlyphRow { label: "Legal", glyphs: "© ® ™ § ¶ † ‡ ‰ ‱" },
            ],
        },
        GlyphSection {
            title: "Combining / Decorative Text",
            color: Color::Cyan,
            rows: vec![
                GlyphRow { label: "Superscripts", glyphs: "⁰ ¹ ² ³ ⁴ ⁵ ⁶ ⁷ ⁸ ⁹ ⁺ ⁻ ⁼ ⁽ ⁾" },
                GlyphRow { label: "Subscripts", glyphs: "₀ ₁ ₂ ₃ ₄ ₅ ₆ ₇ ₈ ₉ ₊ ₋ ₌ ₍ ₎" },
                GlyphRow { label: "Fractions", glyphs: "½ ⅓ ⅔ ¼ ¾ ⅕ ⅖ ⅗ ⅘ ⅙ ⅚ ⅛ ⅜ ⅝ ⅞" },
                GlyphRow { label: "Roman numerals", glyphs: "Ⅰ Ⅱ Ⅲ Ⅳ Ⅴ Ⅵ Ⅶ Ⅷ Ⅸ Ⅹ Ⅺ Ⅻ" },
                GlyphRow { label: "Circled numbers", glyphs: "① ② ③ ④ ⑤ ⑥ ⑦ ⑧ ⑨ ⑩" },
                GlyphRow { label: "Circled letters", glyphs: "Ⓐ Ⓑ Ⓒ Ⓓ Ⓔ Ⓕ Ⓖ Ⓗ Ⓘ Ⓙ" },
            ],
        },
        GlyphSection {
            title: "Practical TUI Patterns",
            color: Color::Green,
            rows: vec![
                GlyphRow {
                    label: "Building blocks",
                    glyphs: "├── branch    └── last    │   continuation",
                },
                GlyphRow {
                    label: "File tree",
                    glyphs: "flotilla/\n├── Cargo.toml\n├── src/\n│   └── main.rs\n├── crates/\n│   ├── core/\n│   │   ├── src/\n│   │   │   ├── model.rs\n│   │   │   ├── data.rs\n│   │   │   └── providers/\n│   │   │       ├── mod.rs\n│   │   │       ├── git.rs\n│   │   │       └── github.rs\n│   │   └── Cargo.toml\n│   └── tui/\n│       ├── src/\n│       │   ├── app/\n│       │   │   ├── mod.rs\n│       │   │   └── intent.rs\n│       │   └── ui.rs\n│       └── Cargo.toml\n└── examples/\n    └── glyph_showcase.rs",
                },
                GlyphRow {
                    label: "With icons",
                    glyphs: "📁 src/\n├── 📄 main.rs\n├── 📄 lib.rs\n├── 📁 providers/\n│   ├── 📄 mod.rs\n│   └── 📄 git.rs\n└── 📄 config.rs",
                },
                GlyphRow {
                    label: "With status",
                    glyphs: "├── ✓ main.rs\n├── ✗ lib.rs        ← compile error\n├── ● config.rs     ← modified\n├── ○ data.rs\n└── ◐ model.rs      ← partially staged",
                },
                GlyphRow {
                    label: "Dotted tree",
                    glyphs: "┊╌╌ optional/\n┊   ┊╌╌ maybe.rs\n┊   └╌╌ perhaps.rs\n└╌╌ definitely.rs",
                },
                GlyphRow {
                    label: "Breadcrumb",
                    glyphs: "Home › Settings › Display",
                },
                GlyphRow {
                    label: "Tab bar",
                    glyphs: "│ Tab 1 │ Tab 2 │ Tab 3 │",
                },
                GlyphRow {
                    label: "Progress bar",
                    glyphs: "[████████░░░░░░░░] 50%",
                },
                GlyphRow {
                    label: "Status line",
                    glyphs: "✓ Pass  ✗ Fail  ● Running  ○ Pending  ◐ Partial",
                },
                GlyphRow {
                    label: "Dividers",
                    glyphs: "──────  ━━━━━━  ╌╌╌╌╌╌  ┄┄┄┄┄┄  ⋯⋯⋯⋯⋯⋯  ═══════",
                },
                GlyphRow {
                    label: "Keycap hints",
                    glyphs: "[q] Quit  [j/k] Navigate  [Enter] Select  [?] Help",
                },
            ],
        },
    ]
}

impl App {
    fn new() -> Self {
        Self {
            scroll: 0,
            sections: build_sections(),
        }
    }

    fn total_lines(&self) -> u16 {
        let mut count: u16 = 0;
        for section in &self.sections {
            count += 2; // title + blank line before content
            for row in &section.rows {
                count += row.glyphs.lines().count() as u16;
            }
            count += 1; // blank after section
        }
        count
    }

    fn render_content(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        for section in &self.sections {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("━━ {} ", section.title),
                    Style::default().fg(section.color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    "━".repeat(60),
                    Style::default().fg(section.color).add_modifier(Modifier::DIM),
                ),
            ]));
            lines.push(Line::default());

            for row in &section.rows {
                let glyph_lines: Vec<&str> = row.glyphs.lines().collect();
                for (i, glyph_line) in glyph_lines.iter().enumerate() {
                    let label = if i == 0 {
                        format!("  {:<22} ", row.label)
                    } else {
                        " ".repeat(25)
                    };
                    lines.push(Line::from(vec![
                        Span::styled(label, Style::default().add_modifier(Modifier::DIM)),
                        Span::raw(*glyph_line),
                    ]));
                }
            }
            lines.push(Line::default());
        }
        lines
    }

    fn draw(&self, frame: &mut Frame) {
        let area = frame.area();

        let [header_area, main_area, footer_area] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0), Constraint::Length(1)])
                .areas(area);

        // Header
        let header = Paragraph::new(Line::from(vec![
            Span::styled(
                " Glyph Showcase ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  Interesting glyphs for terminal UIs"),
        ]))
        .block(Block::default().borders(Borders::BOTTOM));
        frame.render_widget(header, header_area);

        // Main content
        let content_lines = self.render_content();
        let paragraph = Paragraph::new(content_lines)
            .scroll((self.scroll, 0))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::RIGHT));
        frame.render_widget(paragraph, main_area);

        // Scrollbar
        let total = self.total_lines();
        let mut scrollbar_state = ScrollbarState::new(total as usize)
            .position(self.scroll as usize)
            .viewport_content_length(main_area.height as usize);
        frame.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            main_area,
            &mut scrollbar_state,
        );

        // Footer
        let footer = Paragraph::new(Line::from(vec![
            Span::styled(" j/↓ ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" Down  "),
            Span::styled(" k/↑ ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" Up  "),
            Span::styled(" PgDn/PgUp ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" Page  "),
            Span::styled(" Home/End ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" Limits  "),
            Span::styled(" q/Esc ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::raw(" Quit"),
        ]));
        frame.render_widget(footer, footer_area);
    }

    fn scroll_down(&mut self, amount: u16, viewport: u16) {
        let max = self.total_lines().saturating_sub(viewport);
        self.scroll = (self.scroll + amount).min(max);
    }

    fn scroll_up(&mut self, amount: u16) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    fn handle_event(&mut self, viewport: Rect) -> io::Result<bool> {
        if let Event::Key(key) = event::read()? {
            if key.kind != KeyEventKind::Press {
                return Ok(false);
            }
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(true),
                KeyCode::Down | KeyCode::Char('j') => self.scroll_down(1, viewport.height),
                KeyCode::Up | KeyCode::Char('k') => self.scroll_up(1),
                KeyCode::PageDown | KeyCode::Char(' ') => self.scroll_down(viewport.height, viewport.height),
                KeyCode::PageUp => self.scroll_up(viewport.height),
                KeyCode::Home | KeyCode::Char('g') => self.scroll = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    let max = self.total_lines().saturating_sub(viewport.height);
                    self.scroll = max;
                }
                _ => {}
            }
        }
        Ok(false)
    }
}

fn main() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let mut app = App::new();

    loop {
        let viewport = terminal.get_frame().area();
        terminal.draw(|frame| app.draw(frame))?;
        if app.handle_event(viewport)? {
            break;
        }
    }

    ratatui::restore();
    Ok(())
}
