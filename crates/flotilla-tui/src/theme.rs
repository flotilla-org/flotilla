use flotilla_protocol::WorkItemKind;
use ratatui::style::{Color, Modifier, Style};

// ---------------------------------------------------------------------------
// Text transform
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    Uppercase,
    Capitalize,
    AsIs,
}

impl TextTransform {
    pub fn apply(&self, text: &str) -> String {
        match self {
            Self::Uppercase => text.to_uppercase(),
            Self::Capitalize => {
                let mut chars = text.chars();
                match chars.next() {
                    None => String::new(),
                    Some(first) => {
                        let mut s = first.to_uppercase().to_string();
                        s.extend(chars);
                        s
                    }
                }
            }
            Self::AsIs => text.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// Bar chrome
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BarKind {
    Pipe,
    Chevron,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BarSiteStyle {
    pub kind: BarKind,
    pub label_transform: TextTransform,
}

impl BarSiteStyle {
    pub fn transform_label(&self, text: &str) -> String {
        self.label_transform.apply(text)
    }
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Theme {
    pub name: &'static str,
    // Chrome
    pub tab_active: Color,
    pub tab_inactive: Color,
    pub border: Color,
    pub row_highlight: Color,
    pub multi_select_bg: Color,
    pub section_header: Color,
    pub muted: Color,
    // Logo tab
    pub logo_fg: Color,
    pub logo_bg: Color,
    pub logo_config_bg: Color,
    // Work item kinds
    pub checkout: Color,
    pub session: Color,
    pub change_request: Color,
    pub issue: Color,
    pub remote_branch: Color,
    pub workspace: Color,
    // Semantic
    pub branch: Color,
    pub path: Color,
    pub source: Color,
    pub git_status: Color,
    pub error: Color,
    pub warning: Color,
    pub info: Color,
    // Interactive
    pub action_highlight: Color,
    pub input_text: Color,
    // Status
    pub status_ok: Color,
    pub status_error: Color,
    // Surfaces
    pub base: Color,
    pub surface: Color,
    pub text: Color,
    pub subtext: Color,
    // Shimmer
    pub shimmer_base: Color,
    pub shimmer_highlight: Color,
    // Bar chrome
    pub bar_bg: Color,
    pub key_hint: Color,
    pub key_chip_bg: Color,
    pub key_chip_fg: Color,
    // Bar site styling
    pub tab_bar: BarSiteStyle,
    pub status_bar: BarSiteStyle,
}

impl Theme {
    pub fn catppuccin_mocha() -> Self {
        let p = &catppuccin::PALETTE.mocha.colors;
        Self {
            name: "catppuccin-mocha",
            // Chrome
            tab_active: p.sapphire.into(),
            tab_inactive: p.overlay0.into(),
            border: p.surface1.into(),
            row_highlight: p.surface0.into(),
            multi_select_bg: p.surface1.into(),
            section_header: p.yellow.into(),
            muted: p.overlay0.into(),
            // Logo tab
            logo_fg: p.crust.into(),
            logo_bg: p.sapphire.into(),
            logo_config_bg: p.text.into(),
            // Work item kinds
            checkout: p.green.into(),
            session: p.mauve.into(),
            change_request: p.blue.into(),
            issue: p.yellow.into(),
            remote_branch: p.overlay0.into(),
            workspace: p.green.into(),
            // Semantic
            branch: p.teal.into(),
            path: p.subtext0.into(),
            source: p.lavender.into(),
            git_status: p.red.into(),
            error: p.red.into(),
            warning: p.yellow.into(),
            info: p.blue.into(),
            // Interactive
            action_highlight: p.blue.into(),
            input_text: p.teal.into(),
            // Status
            status_ok: p.green.into(),
            status_error: p.red.into(),
            // Surfaces
            base: p.base.into(),
            surface: p.surface0.into(),
            text: p.text.into(),
            subtext: p.subtext0.into(),
            // Shimmer
            shimmer_base: p.peach.into(),
            shimmer_highlight: p.yellow.into(),
            // Bar chrome
            bar_bg: p.crust.into(),
            key_hint: p.peach.into(),
            key_chip_bg: p.surface1.into(),
            key_chip_fg: p.crust.into(),
            // Bar site styling
            tab_bar: BarSiteStyle { kind: BarKind::Chevron, label_transform: TextTransform::AsIs },
            status_bar: BarSiteStyle { kind: BarKind::Chevron, label_transform: TextTransform::Uppercase },
        }
    }

    pub fn classic() -> Self {
        Self {
            name: "classic",
            // Chrome
            tab_active: Color::Cyan,
            tab_inactive: Color::DarkGray,
            border: Color::DarkGray,
            row_highlight: Color::DarkGray,
            multi_select_bg: Color::Indexed(236),
            section_header: Color::Yellow,
            muted: Color::DarkGray,
            // Logo tab
            logo_fg: Color::Black,
            logo_bg: Color::Cyan,
            logo_config_bg: Color::White,
            // Work item kinds
            checkout: Color::Green,
            session: Color::Magenta,
            change_request: Color::Blue,
            issue: Color::Yellow,
            remote_branch: Color::DarkGray,
            workspace: Color::Green,
            // Semantic
            branch: Color::Cyan,
            path: Color::Indexed(245),
            source: Color::Indexed(67),
            git_status: Color::Red,
            error: Color::Red,
            warning: Color::Yellow,
            info: Color::DarkGray,
            // Interactive
            action_highlight: Color::Blue,
            input_text: Color::Cyan,
            // Status
            status_ok: Color::Green,
            status_error: Color::Indexed(203),
            // Surfaces
            base: Color::Reset,
            surface: Color::DarkGray,
            text: Color::White,
            subtext: Color::DarkGray,
            // Shimmer
            shimmer_base: Color::Rgb(140, 130, 40),
            shimmer_highlight: Color::Rgb(255, 240, 120),
            // Bar chrome
            bar_bg: Color::Black,
            key_hint: Color::Indexed(208),
            key_chip_bg: Color::DarkGray,
            key_chip_fg: Color::Black,
            // Bar site styling
            tab_bar: BarSiteStyle { kind: BarKind::Pipe, label_transform: TextTransform::AsIs },
            status_bar: BarSiteStyle { kind: BarKind::Chevron, label_transform: TextTransform::Uppercase },
        }
    }

    // -------------------------------------------------------------------
    // Style-producing methods
    // -------------------------------------------------------------------

    pub fn logo_style(&self, config_mode: bool) -> Style {
        let bg = if config_mode { self.logo_config_bg } else { self.logo_bg };
        Style::default().fg(self.logo_fg).bg(bg).add_modifier(Modifier::BOLD)
    }

    pub fn tab_style(&self, active: bool, dragging: bool) -> Style {
        if active && dragging {
            Style::default().fg(self.tab_active).add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else if active {
            Style::default().fg(self.tab_active).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(self.tab_inactive)
        }
    }

    pub fn work_item_color(&self, kind: &WorkItemKind) -> Color {
        match kind {
            WorkItemKind::Checkout => self.checkout,
            WorkItemKind::AttachableSet => self.checkout,
            WorkItemKind::Session => self.session,
            WorkItemKind::ChangeRequest => self.change_request,
            WorkItemKind::Issue => self.issue,
            WorkItemKind::RemoteBranch => self.remote_branch,
            WorkItemKind::Agent => self.session,
        }
    }

    pub fn header_style(&self) -> Style {
        Style::default().fg(self.section_header).add_modifier(Modifier::BOLD)
    }

    /// Style for bordered block chrome (border lines and title text).
    pub fn block_style(&self) -> Style {
        Style::default().fg(self.border)
    }

    pub fn log_level_style(&self, level: &str) -> Style {
        let color = match level {
            "ERROR" => self.error,
            "WARN" => self.warning,
            "DEBUG" => self.branch,
            "INFO" => self.info,
            _ => self.muted,
        };
        Style::default().fg(color)
    }

    pub fn change_request_status_color(&self, status: &str) -> Color {
        let lower = status.to_ascii_lowercase();
        match lower.as_str() {
            "merged" => self.status_ok,
            "closed" => self.warning,
            _ => self.error,
        }
    }

    pub fn status_style(&self, ok: bool) -> Style {
        let color = if ok { self.status_ok } else { self.status_error };
        Style::default().fg(color)
    }

    pub fn peer_status_color(&self, state: &str) -> Color {
        match state {
            "Connected" => self.status_ok,
            "Disconnected" | "Rejected" => self.error,
            "Connecting" | "Reconnecting" => self.warning,
            _ => self.muted,
        }
    }
}

// ---------------------------------------------------------------------------
// Theme registry
// ---------------------------------------------------------------------------

/// A named theme constructor: `(name, constructor)`.
pub type ThemeEntry = (&'static str, fn() -> Theme);

/// Returns the list of all built-in themes as `(name, constructor)` pairs.
pub fn available_themes() -> &'static [ThemeEntry] {
    &[("classic", Theme::classic), ("catppuccin-mocha", Theme::catppuccin_mocha)]
}

/// Looks up a theme by name (case-insensitive). Falls back to `classic`.
pub fn theme_by_name(name: &str) -> Theme {
    available_themes().iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, ctor)| ctor()).unwrap_or_else(Theme::classic)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- TextTransform ----

    #[test]
    fn text_transform_uppercase() {
        assert_eq!(TextTransform::Uppercase.apply("hello"), "HELLO");
    }

    #[test]
    fn text_transform_titlecase() {
        assert_eq!(TextTransform::Capitalize.apply("hello world"), "Hello world");
    }

    #[test]
    fn text_transform_titlecase_empty() {
        assert_eq!(TextTransform::Capitalize.apply(""), "");
    }

    #[test]
    fn text_transform_as_is() {
        assert_eq!(TextTransform::AsIs.apply("Hello"), "Hello");
    }

    // ---- Classic theme field spot-checks ----

    #[test]
    fn classic_name() {
        assert_eq!(Theme::classic().name, "classic");
    }

    #[test]
    fn classic_tab_colours() {
        let t = Theme::classic();
        assert_eq!(t.tab_active, Color::Cyan);
        assert_eq!(t.tab_inactive, Color::DarkGray);
    }

    #[test]
    fn classic_work_item_colours() {
        let t = Theme::classic();
        assert_eq!(t.checkout, Color::Green);
        assert_eq!(t.session, Color::Magenta);
        assert_eq!(t.change_request, Color::Blue);
        assert_eq!(t.issue, Color::Yellow);
        assert_eq!(t.remote_branch, Color::DarkGray);
    }

    #[test]
    fn classic_indexed_colours() {
        let t = Theme::classic();
        assert_eq!(t.multi_select_bg, Color::Indexed(236));
        assert_eq!(t.path, Color::Indexed(245));
        assert_eq!(t.source, Color::Indexed(67));
        assert_eq!(t.status_error, Color::Indexed(203));
        assert_eq!(t.key_hint, Color::Indexed(208));
    }

    #[test]
    fn classic_logo() {
        let t = Theme::classic();
        assert_eq!(t.logo_fg, Color::Black);
        assert_eq!(t.logo_bg, Color::Cyan);
        assert_eq!(t.logo_config_bg, Color::White);
    }

    #[test]
    fn classic_shimmer() {
        let t = Theme::classic();
        assert_eq!(t.shimmer_base, Color::Rgb(140, 130, 40));
        assert_eq!(t.shimmer_highlight, Color::Rgb(255, 240, 120));
    }

    #[test]
    fn classic_bar_styles() {
        let t = Theme::classic();
        assert_eq!(t.tab_bar.kind, BarKind::Pipe);
        assert_eq!(t.tab_bar.label_transform, TextTransform::AsIs);
        assert_eq!(t.status_bar.kind, BarKind::Chevron);
        assert_eq!(t.status_bar.label_transform, TextTransform::Uppercase);
    }

    // ---- Catppuccin Mocha ----

    #[test]
    fn catppuccin_mocha_name() {
        assert_eq!(Theme::catppuccin_mocha().name, "catppuccin-mocha");
    }

    #[test]
    fn catppuccin_mocha_uses_rgb_colours() {
        let t = Theme::catppuccin_mocha();
        // Catppuccin produces Rgb values, not named terminal colours
        assert!(matches!(t.tab_active, Color::Rgb(_, _, _)));
        assert!(matches!(t.base, Color::Rgb(_, _, _)));
        assert!(matches!(t.text, Color::Rgb(_, _, _)));
    }

    #[test]
    fn catppuccin_mocha_differs_from_classic() {
        let c = Theme::classic();
        let m = Theme::catppuccin_mocha();
        assert_ne!(c.name, m.name);
        assert_ne!(c.tab_active, m.tab_active);
        assert_ne!(c.base, m.base);
    }

    // ---- Theme registry ----

    #[test]
    fn available_themes_length_and_names() {
        let themes = available_themes();
        assert_eq!(themes.len(), 2);
        let names: Vec<&str> = themes.iter().map(|(name, _)| *name).collect();
        assert!(names.contains(&"classic"));
        assert!(names.contains(&"catppuccin-mocha"));
    }

    #[test]
    fn theme_by_name_found() {
        assert_eq!(theme_by_name("catppuccin-mocha").name, "catppuccin-mocha");
    }

    #[test]
    fn theme_by_name_case_insensitive() {
        assert_eq!(theme_by_name("Catppuccin-Mocha").name, "catppuccin-mocha");
    }

    #[test]
    fn theme_by_name_fallback() {
        assert_eq!(theme_by_name("nonexistent").name, "classic");
    }

    // ---- Style-producing methods ----

    #[test]
    fn logo_style_normal() {
        let t = Theme::classic();
        let s = t.logo_style(false);
        assert_eq!(s.fg, Some(Color::Black));
        assert_eq!(s.bg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn logo_style_config_mode() {
        let t = Theme::classic();
        let s = t.logo_style(true);
        assert_eq!(s.fg, Some(Color::Black));
        assert_eq!(s.bg, Some(Color::White));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_style_active() {
        let t = Theme::classic();
        let s = t.tab_style(true, false);
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert!(!s.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn tab_style_inactive() {
        let t = Theme::classic();
        let s = t.tab_style(false, false);
        assert_eq!(s.fg, Some(Color::DarkGray));
        assert!(!s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_style_active_dragging() {
        let t = Theme::classic();
        let s = t.tab_style(true, true);
        assert_eq!(s.fg, Some(Color::Cyan));
        assert!(s.add_modifier.contains(Modifier::BOLD));
        assert!(s.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn work_item_color_all_kinds() {
        let t = Theme::classic();
        assert_eq!(t.work_item_color(&WorkItemKind::Checkout), Color::Green);
        assert_eq!(t.work_item_color(&WorkItemKind::Session), Color::Magenta);
        assert_eq!(t.work_item_color(&WorkItemKind::ChangeRequest), Color::Blue);
        assert_eq!(t.work_item_color(&WorkItemKind::Issue), Color::Yellow);
        assert_eq!(t.work_item_color(&WorkItemKind::RemoteBranch), Color::DarkGray);
    }

    #[test]
    fn header_style_is_bold_section_header() {
        let t = Theme::classic();
        let s = t.header_style();
        assert_eq!(s.fg, Some(Color::Yellow));
        assert!(s.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn log_level_style_error() {
        let t = Theme::classic();
        assert_eq!(t.log_level_style("ERROR").fg, Some(Color::Red));
    }

    #[test]
    fn log_level_style_warn() {
        let t = Theme::classic();
        assert_eq!(t.log_level_style("WARN").fg, Some(Color::Yellow));
    }

    #[test]
    fn log_level_style_debug() {
        let t = Theme::classic();
        assert_eq!(t.log_level_style("DEBUG").fg, Some(Color::Cyan));
    }

    #[test]
    fn log_level_style_info() {
        let t = Theme::classic();
        assert_eq!(t.log_level_style("INFO").fg, Some(Color::DarkGray));
    }

    #[test]
    fn log_level_style_unknown() {
        let t = Theme::classic();
        assert_eq!(t.log_level_style("TRACE").fg, Some(Color::DarkGray));
    }

    #[test]
    fn change_request_status_merged() {
        let t = Theme::classic();
        assert_eq!(t.change_request_status_color("merged"), Color::Green);
        assert_eq!(t.change_request_status_color("MERGED"), Color::Green);
    }

    #[test]
    fn change_request_status_closed() {
        let t = Theme::classic();
        assert_eq!(t.change_request_status_color("closed"), Color::Yellow);
        assert_eq!(t.change_request_status_color("Closed"), Color::Yellow);
    }

    #[test]
    fn change_request_status_open() {
        let t = Theme::classic();
        assert_eq!(t.change_request_status_color("open"), Color::Red);
        assert_eq!(t.change_request_status_color("Open"), Color::Red);
    }

    #[test]
    fn status_style_ok() {
        let t = Theme::classic();
        assert_eq!(t.status_style(true).fg, Some(Color::Green));
    }

    #[test]
    fn status_style_error() {
        let t = Theme::classic();
        assert_eq!(t.status_style(false).fg, Some(Color::Indexed(203)));
    }

    #[test]
    fn peer_status_connected() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("Connected"), Color::Green);
    }

    #[test]
    fn peer_status_disconnected() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("Disconnected"), Color::Red);
    }

    #[test]
    fn peer_status_rejected() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("Rejected"), Color::Red);
    }

    #[test]
    fn peer_status_connecting() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("Connecting"), Color::Yellow);
    }

    #[test]
    fn peer_status_reconnecting() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("Reconnecting"), Color::Yellow);
    }

    #[test]
    fn peer_status_unknown() {
        let t = Theme::classic();
        assert_eq!(t.peer_status_color("SomeOther"), Color::DarkGray);
    }

    #[test]
    fn transform_label_uses_site_transform() {
        let t = Theme::classic();
        assert_eq!(t.status_bar.transform_label("hello"), "HELLO");
        assert_eq!(t.tab_bar.transform_label("Hello"), "Hello");
    }
}
