use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

use crate::theme::{BarSiteStyle, Theme};

/// Client-provided data for one segment in a bar.
pub struct SegmentItem {
    pub label: String,
    pub key_hint: Option<String>,
    pub active: bool,
    pub dragging: bool,
    pub style_override: Option<Style>,
}

/// Bundles rendered spans with their computed display width.
pub struct RenderedItem {
    pub spans: Vec<Span<'static>>,
    pub width: usize,
}

impl RenderedItem {
    pub fn from_spans(spans: Vec<Span<'static>>) -> Self {
        let width = spans.iter().map(|s| s.content.as_ref().width()).sum();
        Self { spans, width }
    }

    pub fn empty() -> Self {
        Self { spans: vec![], width: 0 }
    }
}

/// A clickable region produced by rendering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HitRegion {
    pub area: Rect,
    pub index: usize,
}

/// Provides all visual decisions for a segment bar.
pub trait BarStyle {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem;
    fn separator(&self) -> RenderedItem;
    fn background_fill(&self) -> Option<Style>;
}

/// Render a segment bar into the buffer and return hit regions.
pub fn render(items: &[SegmentItem], style: &dyn BarStyle, area: Rect, buf: &mut Buffer) -> Vec<HitRegion> {
    let mut hits = Vec::with_capacity(items.len());
    let mut x = area.x;
    let max_x = area.x + area.width;

    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            let sep = style.separator();
            for span in &sep.spans {
                let w = span.content.as_ref().width() as u16;
                if x + w > max_x {
                    break;
                }
                buf.set_span(x, area.y, span, w);
                x += w;
            }
        }

        let rendered = style.render_item(item);
        let item_start = x;
        for span in &rendered.spans {
            let w = span.content.as_ref().width() as u16;
            if x + w > max_x {
                break;
            }
            buf.set_span(x, area.y, span, w);
            x += w;
        }
        let item_end = x;
        if item_end > item_start {
            hits.push(HitRegion { area: Rect::new(item_start, area.y, item_end - item_start, 1), index: i });
        }
    }

    if let Some(bg_style) = style.background_fill() {
        while x < max_x {
            buf[(x, area.y)].set_style(bg_style);
            buf[(x, area.y)].set_symbol(" ");
            x += 1;
        }
    }

    hits
}

/// Powerline chevron character (U+E0B0).
const CHEVRON: &str = "\u{e0b0}";

/// Theme-aware tab bar style: reads colours from a `Theme`.
pub struct ThemedTabBarStyle<'a> {
    pub theme: &'a Theme,
    pub site: &'a BarSiteStyle,
}

impl BarStyle for ThemedTabBarStyle<'_> {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        // Precedence: override > active+dragging > active > inactive.
        let style = if let Some(ov) = item.style_override { ov } else { self.theme.tab_style(item.active, item.dragging) };
        let label = self.site.transform_label(&item.label);

        let mut spans = Vec::new();
        if let Some(key) = &item.key_hint {
            let key_style = Style::default().fg(self.theme.key_hint).add_modifier(style.add_modifier);
            spans.push(Span::styled("<", style));
            spans.push(Span::styled(key.clone(), key_style));
            spans.push(Span::styled("> ", style));
        }
        spans.push(Span::styled(label, style));

        RenderedItem::from_spans(spans)
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::from_spans(vec![Span::styled(" | ", Style::default().fg(self.theme.muted))])
    }

    fn background_fill(&self) -> Option<Style> {
        None
    }
}

/// Theme-aware ribbon style with chevron-delimited key chips.
pub struct ThemedRibbonStyle<'a> {
    pub theme: &'a Theme,
    pub site: &'a BarSiteStyle,
}

impl BarStyle for ThemedRibbonStyle<'_> {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let label = self.site.transform_label(&item.label);

        // Resolve chip colours: override > active+dragging > active > inactive.
        let (chip_bg, chip_fg, modifiers) = if let Some(ov) = item.style_override {
            (ov.bg.unwrap_or(self.theme.key_chip_bg), ov.fg.unwrap_or(self.theme.key_chip_fg), ov.add_modifier)
        } else if item.active {
            let mods = if item.dragging { Modifier::BOLD | Modifier::UNDERLINED } else { Modifier::BOLD };
            (self.theme.tab_active, self.theme.key_chip_fg, mods)
        } else {
            (self.theme.key_chip_bg, self.theme.key_chip_fg, Modifier::BOLD)
        };

        let chip_style = Style::default().fg(chip_fg).bg(chip_bg).add_modifier(modifiers);
        let key_style = Style::default().fg(self.theme.key_hint).bg(chip_bg).add_modifier(modifiers);

        let mut spans = vec![Span::styled(CHEVRON, Style::default().fg(self.theme.bar_bg).bg(chip_bg))];

        if let Some(key) = &item.key_hint {
            spans.push(Span::styled(" ", chip_style));
            spans.push(Span::styled("<", chip_style));
            spans.push(Span::styled(key.clone(), key_style));
            spans.push(Span::styled(">", chip_style));
            spans.push(Span::styled(format!(" {label} "), chip_style));
        } else {
            spans.push(Span::styled(format!(" {label} "), chip_style));
        }

        spans.push(Span::styled(CHEVRON, Style::default().fg(chip_bg).bg(self.theme.bar_bg)));

        RenderedItem::from_spans(spans)
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::empty()
    }

    fn background_fill(&self) -> Option<Style> {
        Some(Style::default().fg(self.theme.text).bg(self.theme.bar_bg))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Modifier};

    use super::*;
    use crate::theme::{BarKind, TextTransform};

    /// Find the first span whose content contains `needle`.
    fn find_span<'a>(rendered: &'a RenderedItem, needle: &str) -> &'a Span<'a> {
        rendered.spans.iter().find(|s| s.content.contains(needle)).unwrap_or_else(|| panic!("no span containing {needle:?}"))
    }

    #[test]
    fn rendered_item_from_span() {
        let item = RenderedItem::from_spans(vec![Span::raw("hello"), Span::raw(" world")]);
        assert_eq!(item.width, 11);
        assert_eq!(item.spans.len(), 2);
    }

    #[test]
    fn rendered_item_empty() {
        let item = RenderedItem::empty();
        assert_eq!(item.width, 0);
        assert!(item.spans.is_empty());
    }

    #[test]
    fn render_produces_hit_regions() {
        let items =
            vec![SegmentItem { label: "Alpha".into(), key_hint: None, active: true, dragging: false, style_override: None }, SegmentItem {
                label: "Beta".into(),
                key_hint: None,
                active: false,
                dragging: false,
                style_override: None,
            }];

        struct TestStyle;
        impl BarStyle for TestStyle {
            fn render_item(&self, item: &SegmentItem) -> RenderedItem {
                RenderedItem::from_spans(vec![Span::raw(item.label.clone())])
            }
            fn separator(&self) -> RenderedItem {
                RenderedItem::from_spans(vec![Span::raw(" | ")])
            }
            fn background_fill(&self) -> Option<Style> {
                None
            }
        }

        let mut buf = Buffer::empty(Rect::new(0, 0, 40, 1));
        let hits = render(&items, &TestStyle, Rect::new(0, 0, 40, 1), &mut buf);

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].index, 0);
        assert_eq!(hits[0].area, Rect::new(0, 0, 5, 1));
        assert_eq!(hits[1].index, 1);
        assert_eq!(hits[1].area, Rect::new(8, 0, 4, 1));
    }

    #[test]
    fn tab_style_renders_active_and_inactive() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar };
        let active = SegmentItem { label: "active".into(), key_hint: None, active: true, dragging: false, style_override: None };
        let inactive = SegmentItem { label: "inactive".into(), key_hint: None, active: false, dragging: false, style_override: None };

        let a = style.render_item(&active);
        let i = style.render_item(&inactive);

        assert_eq!(a.width, 6);
        assert_eq!(i.width, 8);
        assert!(a.spans[0].style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_style_applies_style_override() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem {
            label: "[+]".into(),
            key_hint: None,
            active: false,
            dragging: false,
            style_override: Some(Style::default().fg(Color::Green)),
        };
        let rendered = style.render_item(&item);
        assert_eq!(rendered.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn tab_style_separator_width() {
        let theme = crate::theme::Theme::classic();
        let sep = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar }.separator();
        assert_eq!(sep.width, 3);
    }

    #[test]
    fn tab_style_renders_key_hint() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem { label: "open".into(), key_hint: Some("ENT".into()), active: true, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("<"));
        assert!(text.contains("ENT"));
        assert!(text.contains(">"));
        assert!(text.contains("open"));
    }

    #[test]
    fn tab_style_no_key_hint_no_brackets() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem { label: "repo".into(), key_hint: None, active: false, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        assert_eq!(rendered.spans.len(), 1);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert!(!text.contains("<"));
        assert!(!text.contains(">"));
    }

    #[test]
    fn tab_style_key_hint_inherits_dragging_underline() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedTabBarStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem { label: "open".into(), key_hint: Some("ENT".into()), active: true, dragging: true, style_override: None };
        let rendered = style.render_item(&item);
        let key_span = find_span(&rendered, "ENT");
        assert!(key_span.style.add_modifier.contains(Modifier::UNDERLINED));
        assert!(key_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn tab_style_applies_label_transform() {
        let theme = crate::theme::Theme::classic();
        let site = BarSiteStyle { kind: BarKind::Pipe, label_transform: TextTransform::Uppercase };
        let style = ThemedTabBarStyle { theme: &theme, site: &site };
        let item = SegmentItem { label: "hello".into(), key_hint: None, active: false, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert_eq!(text, "HELLO");
    }

    #[test]
    fn render_fills_background() {
        struct FillStyle;
        impl BarStyle for FillStyle {
            fn render_item(&self, item: &SegmentItem) -> RenderedItem {
                RenderedItem::from_spans(vec![Span::raw(item.label.clone())])
            }
            fn separator(&self) -> RenderedItem {
                RenderedItem::empty()
            }
            fn background_fill(&self) -> Option<Style> {
                Some(Style::default().bg(Color::Black))
            }
        }

        let items = vec![SegmentItem { label: "Hi".into(), key_hint: None, active: false, dragging: false, style_override: None }];
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        render(&items, &FillStyle, Rect::new(0, 0, 10, 1), &mut buf);

        // Remaining cells should have black background
        assert_eq!(buf[(5, 0)].bg, Color::Black);
        assert_eq!(buf[(9, 0)].bg, Color::Black);
    }

    #[test]
    fn ribbon_style_renders_with_key_hint() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedRibbonStyle { theme: &theme, site: &theme.status_bar };
        let item = SegmentItem { label: "open".into(), key_hint: Some("ENT".into()), active: false, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        assert_eq!(rendered.spans.len(), 7);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("ENT"));
        assert!(text.contains("OPEN")); // label is uppercased by status_bar transform
    }

    #[test]
    fn ribbon_style_elides_brackets_without_key_hint() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedRibbonStyle { theme: &theme, site: &theme.status_bar };
        let item = SegmentItem { label: "repo".into(), key_hint: None, active: false, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert!(!text.contains("<"));
        assert!(!text.contains(">"));
        // Chevron + " REPO " + chevron = 3 spans
        assert_eq!(rendered.spans.len(), 3);
    }

    #[test]
    fn ribbon_style_active_uses_tab_active_bg() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedRibbonStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem { label: "repo".into(), key_hint: None, active: true, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        let label_span = find_span(&rendered, "repo");
        assert_eq!(label_span.style.bg, Some(theme.tab_active));
        assert!(label_span.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn ribbon_style_dragging_adds_underline() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedRibbonStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem { label: "repo".into(), key_hint: None, active: true, dragging: true, style_override: None };
        let rendered = style.render_item(&item);
        let label_span = find_span(&rendered, "repo");
        assert!(label_span.style.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn ribbon_style_applies_style_override() {
        let theme = crate::theme::Theme::classic();
        let style = ThemedRibbonStyle { theme: &theme, site: &theme.tab_bar };
        let item = SegmentItem {
            label: "logo".into(),
            key_hint: None,
            active: false,
            dragging: false,
            style_override: Some(Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        };
        let rendered = style.render_item(&item);
        let label_span = find_span(&rendered, "logo");
        assert_eq!(label_span.style.fg, Some(Color::Black));
        assert_eq!(label_span.style.bg, Some(Color::Cyan));
        // Left chevron transitions to override bg
        let chevron_span = find_span(&rendered, CHEVRON);
        assert_eq!(chevron_span.style.bg, Some(Color::Cyan));
    }

    #[test]
    fn ribbon_style_separator_is_empty() {
        let theme = crate::theme::Theme::classic();
        let sep = ThemedRibbonStyle { theme: &theme, site: &theme.status_bar }.separator();
        assert_eq!(sep.width, 0);
    }

    #[test]
    fn ribbon_style_fills_background() {
        let theme = crate::theme::Theme::classic();
        assert!(ThemedRibbonStyle { theme: &theme, site: &theme.status_bar }.background_fill().is_some());
    }
}
