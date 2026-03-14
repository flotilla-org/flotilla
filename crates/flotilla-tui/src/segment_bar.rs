use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

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

/// Tab bar style: pipe separators, cyan active, dark gray inactive.
pub struct TabBarStyle;

impl BarStyle for TabBarStyle {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let style = if let Some(override_style) = item.style_override {
            override_style
        } else if item.active && item.dragging {
            Style::default().bold().fg(Color::Cyan).add_modifier(Modifier::UNDERLINED)
        } else if item.active {
            Style::default().bold().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        RenderedItem::from_spans(vec![Span::styled(item.label.clone(), style)])
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::from_spans(vec![Span::styled(" | ", Style::default().fg(Color::DarkGray))])
    }

    fn background_fill(&self) -> Option<Style> {
        None
    }
}

/// Status bar key ribbon style with chevron-delimited key chips.
pub struct RibbonStyle;

impl BarStyle for RibbonStyle {
    fn render_item(&self, item: &SegmentItem) -> RenderedItem {
        let key = item.key_hint.as_deref().unwrap_or("");
        RenderedItem::from_spans(vec![
            Span::styled(CHEVRON, Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled(" ", Style::default().fg(Color::Black).bg(Color::DarkGray)),
            Span::styled("<", Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(key.to_string(), Style::default().fg(Color::Indexed(208)).bg(Color::DarkGray).bold()),
            Span::styled(">", Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(format!(" {} ", item.label), Style::default().fg(Color::Black).bg(Color::DarkGray).bold()),
            Span::styled(CHEVRON, Style::default().fg(Color::DarkGray).bg(Color::Black)),
        ])
    }

    fn separator(&self) -> RenderedItem {
        RenderedItem::empty()
    }

    fn background_fill(&self) -> Option<Style> {
        Some(Style::default().fg(Color::White).bg(Color::Black))
    }
}

#[cfg(test)]
mod tests {
    use ratatui::style::Color;

    use super::*;

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
        let style = TabBarStyle;
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
        let style = TabBarStyle;
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
        let sep = TabBarStyle.separator();
        assert_eq!(sep.width, 3);
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
        let style = RibbonStyle;
        let item = SegmentItem { label: "OPEN".into(), key_hint: Some("ENT".into()), active: false, dragging: false, style_override: None };
        let rendered = style.render_item(&item);
        assert_eq!(rendered.spans.len(), 7);
        let text: String = rendered.spans.iter().map(|s| s.content.as_ref().to_string()).collect();
        assert!(text.contains("ENT"));
        assert!(text.contains("OPEN"));
    }

    #[test]
    fn ribbon_style_separator_is_empty() {
        let sep = RibbonStyle.separator();
        assert_eq!(sep.width, 0);
    }

    #[test]
    fn ribbon_style_fills_background() {
        assert!(RibbonStyle.background_fill().is_some());
    }
}
