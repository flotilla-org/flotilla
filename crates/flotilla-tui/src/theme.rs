use ratatui::style::Color;

// ---------------------------------------------------------------------------
// Text transform
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextTransform {
    Uppercase,
    Titlecase,
    AsIs,
}

impl TextTransform {
    pub fn apply(&self, text: &str) -> String {
        match self {
            Self::Uppercase => text.to_uppercase(),
            Self::Titlecase => {
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
            shimmer_base: p.yellow.into(),
            shimmer_highlight: p.rosewater.into(),
            // Bar chrome
            bar_bg: p.crust.into(),
            key_hint: p.peach.into(),
            key_chip_bg: p.surface1.into(),
            key_chip_fg: p.crust.into(),
            // Bar site styling
            tab_bar: BarSiteStyle { kind: BarKind::Pipe, label_transform: TextTransform::AsIs },
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
}

// ---------------------------------------------------------------------------
// Theme registry
// ---------------------------------------------------------------------------

/// Returns the list of all built-in theme constructors.
pub fn available_themes() -> &'static [fn() -> Theme] {
    &[Theme::classic, Theme::catppuccin_mocha]
}

/// Looks up a theme by name (case-insensitive). Falls back to `classic`.
pub fn theme_by_name(name: &str) -> Theme {
    available_themes()
        .iter()
        .map(|ctor| ctor())
        .find(|t| t.name.eq_ignore_ascii_case(name))
        .unwrap_or_else(Theme::classic)
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
        assert_eq!(TextTransform::Titlecase.apply("hello world"), "Hello world");
    }

    #[test]
    fn text_transform_titlecase_empty() {
        assert_eq!(TextTransform::Titlecase.apply(""), "");
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
        let names: Vec<&str> = themes.iter().map(|ctor| ctor().name).collect();
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
}
