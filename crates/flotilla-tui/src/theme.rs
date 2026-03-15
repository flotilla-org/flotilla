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
}
