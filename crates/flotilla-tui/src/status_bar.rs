use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::layout::Rect;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub const CHEVRON_SEPARATOR: &str = "";
pub const DEFAULT_STATUS_WIDTH_BUDGET: usize = 28;
const MIN_STATUS_WIDTH: usize = 8;
const MIN_TASK_WIDTH: usize = 12;
const SPINNER_FRAMES: [&str; 4] = ["-", "\\", "|", "/"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusBarAction {
    KeyPress { code: KeyCode, modifiers: KeyModifiers },
    ClearError(usize),
}

impl StatusBarAction {
    pub fn key(code: KeyCode) -> Self {
        Self::KeyPress { code, modifiers: KeyModifiers::NONE }
    }

    pub fn shifted(code: KeyCode) -> Self {
        Self::KeyPress { code, modifiers: KeyModifiers::SHIFT }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyChip {
    pub key: String,
    pub label: String,
    pub action: StatusBarAction,
}

impl KeyChip {
    pub fn new(key: &str, label: &str, action: StatusBarAction) -> Self {
        Self { key: key.to_string(), label: label.to_string(), action }
    }

    pub fn content_width(&self) -> usize {
        displayed_width(&self.key) + displayed_width(&self.label) + 5
    }

    pub fn ribbon_width(&self) -> usize {
        self.content_width() + 2
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSection {
    pub description: String,
    pub spinner_index: usize,
}

impl TaskSection {
    pub fn new(description: &str, spinner_index: usize) -> Self {
        Self { description: description.to_string(), spinner_index }
    }

    fn render(&self) -> String {
        let spinner = SPINNER_FRAMES[self.spinner_index % SPINNER_FRAMES.len()];
        format!("{spinner} {}", self.description)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StatusSection {
    Plain(String),
    Error { id: usize, text: String },
}

impl StatusSection {
    pub fn plain(text: &str) -> Self {
        Self::Plain(text.to_string())
    }

    pub fn error(id: usize, text: &str) -> Self {
        Self::Error { id, text: text.to_string() }
    }

    pub fn dismiss_id(&self) -> Option<usize> {
        match self {
            Self::Error { id, .. } => Some(*id),
            Self::Plain(_) => None,
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Plain(text) => text.clone(),
            Self::Error { text, .. } => format!("{text} ×"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModeIndicator {
    pub icon: String,
    pub label: String,
    pub action: StatusBarAction,
}

impl ModeIndicator {
    pub fn new(icon: &str, label: &str, action: StatusBarAction) -> Self {
        Self { icon: icon.to_string(), label: label.to_string(), action }
    }

    /// Display width: " icon label " (space, icon, space, label, space).
    pub fn width(&self) -> usize {
        displayed_width(&self.icon) + displayed_width(&self.label) + 3
    }
}

fn total_mode_width(indicators: &[ModeIndicator]) -> usize {
    indicators.iter().map(ModeIndicator::width).sum()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusBarInput {
    pub width: usize,
    pub preferred_status_width: usize,
    pub keys_visible: bool,
    pub status: StatusSection,
    pub task: Option<TaskSection>,
    pub keys: Vec<KeyChip>,
    pub mode_indicators: Vec<ModeIndicator>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusBarModel {
    pub status_text: String,
    pub visible_keys: Vec<KeyChip>,
    pub mode_indicators: Vec<ModeIndicator>,
    pub task_text: String,
    pub keys_start: usize,
    pub mode_start: usize,
    pub task_start: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusBarTarget {
    pub area: Rect,
    pub action: StatusBarAction,
}

impl StatusBarTarget {
    pub fn new(area: Rect, action: StatusBarAction) -> Self {
        Self { area, action }
    }

    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.area.x
            && x < self.area.x.saturating_add(self.area.width)
            && y >= self.area.y
            && y < self.area.y.saturating_add(self.area.height)
    }
}

/// Gap (in columns) between action key chips and mode indicators.
const MODE_GAP: usize = 2;

impl StatusBarModel {
    pub fn build(input: StatusBarInput) -> Self {
        let mut status_text = input.status.render();
        let mut task_text = input.task.as_ref().map(TaskSection::render).unwrap_or_default();
        let mut visible_keys = if input.keys_visible { input.keys.clone() } else { vec![] };
        let mode_width = total_mode_width(&input.mode_indicators);

        loop {
            let keys_width = total_keys_width(&visible_keys);
            let task_width = displayed_width(&task_text);
            let status_width = displayed_width(&status_text);
            let reserved_status_width = status_width.max(input.preferred_status_width.min(input.width));
            let gap = if keys_width > 0 && mode_width > 0 { MODE_GAP } else { 0 };

            if reserved_status_width + keys_width + gap + mode_width + task_width <= input.width {
                let task_start = input.width.saturating_sub(task_width);
                let after_keys = reserved_status_width + keys_width;
                let mode_start = if mode_width == 0 { task_start } else { after_keys + gap };
                let keys_start = if keys_width == 0 && mode_width == 0 { task_start } else { reserved_status_width };

                return Self {
                    status_text,
                    visible_keys,
                    mode_indicators: input.mode_indicators,
                    task_text,
                    keys_start,
                    mode_start,
                    task_start,
                };
            }

            if !visible_keys.is_empty() {
                visible_keys.pop();
                continue;
            }
            if !task_text.is_empty() && task_width > MIN_TASK_WIDTH {
                task_text = ellipsize(&task_text, task_width.saturating_sub(1));
                continue;
            }
            if status_width > MIN_STATUS_WIDTH {
                status_text = ellipsize(&status_text, status_width.saturating_sub(1));
                continue;
            }

            let fallback_keys_start = status_width.min(input.width);
            return Self {
                status_text,
                visible_keys,
                mode_indicators: input.mode_indicators,
                task_text,
                keys_start: fallback_keys_start,
                mode_start: fallback_keys_start,
                task_start: input.width,
            };
        }
    }
}

fn total_keys_width(keys: &[KeyChip]) -> usize {
    keys.iter().map(KeyChip::ribbon_width).sum()
}

fn displayed_width(text: &str) -> usize {
    text.width()
}

fn ellipsize(text: &str, max_width: usize) -> String {
    if displayed_width(text) <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }

    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let glyph_width = ch.width().unwrap_or(0);
        if used + glyph_width >= max_width {
            break;
        }
        result.push(ch);
        used += glyph_width;
    }
    result.push('…');
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hides_keys_before_truncating_task_or_status() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 48,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET,
            keys_visible: true,
            status: StatusSection::plain("Connected"),
            task: Some(TaskSection::new("Refreshing repository...", 0)),
            keys: vec![
                KeyChip::new("enter", "Open", StatusBarAction::key(KeyCode::Enter)),
                KeyChip::new("/", "Search", StatusBarAction::key(KeyCode::Char('/'))),
                KeyChip::new("q", "Quit", StatusBarAction::key(KeyCode::Char('q'))),
            ],
            mode_indicators: vec![],
        });

        assert!(model.visible_keys.len() < 3);
        assert!(model.task_text.contains("Refreshing"));
        assert!(model.status_text.contains("Connected"));
    }

    #[test]
    fn hidden_keys_remove_middle_section_entirely() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 80,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET,
            keys_visible: false,
            status: StatusSection::plain("Ready"),
            task: None,
            keys: vec![KeyChip::new("q", "Quit", StatusBarAction::key(KeyCode::Char('q')))],
            mode_indicators: vec![],
        });

        assert!(model.visible_keys.is_empty());
        assert_eq!(model.keys_start, 80);
    }

    #[test]
    fn reserves_status_budget_before_keys_when_space_allows() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 80,
            preferred_status_width: 28,
            keys_visible: true,
            status: StatusSection::plain("Ready"),
            task: None,
            keys: vec![KeyChip::new("/", "Search", StatusBarAction::key(KeyCode::Char('/')))],
            mode_indicators: vec![],
        });

        assert_eq!(model.keys_start, 28);
    }

    #[test]
    fn task_is_right_aligned() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 80,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET,
            keys_visible: true,
            status: StatusSection::plain("Ready"),
            task: Some(TaskSection::new("Generating branch name...", 0)),
            keys: vec![KeyChip::new("/", "Search", StatusBarAction::key(KeyCode::Char('/')))],
            mode_indicators: vec![],
        });

        assert_eq!(model.task_start + displayed_width(&model.task_text), 80);
    }

    #[test]
    fn mode_indicators_appear_after_keys() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 80,
            preferred_status_width: 28,
            keys_visible: true,
            status: StatusSection::plain("Ready"),
            task: None,
            keys: vec![KeyChip::new("/", "Search", StatusBarAction::key(KeyCode::Char('/')))],
            mode_indicators: vec![
                ModeIndicator::new("□", "zoom", StatusBarAction::key(KeyCode::Char('l'))),
                ModeIndicator::new("@", "local", StatusBarAction::key(KeyCode::Char('h'))),
            ],
        });

        // keys at 28, mode after keys + gap
        assert_eq!(model.keys_start, 28);
        let keys_end = model.keys_start + total_keys_width(&model.visible_keys);
        assert_eq!(model.mode_start, keys_end + MODE_GAP);
    }

    #[test]
    fn mode_indicators_shed_keys_first() {
        let model = StatusBarModel::build(StatusBarInput {
            width: 50,
            preferred_status_width: DEFAULT_STATUS_WIDTH_BUDGET,
            keys_visible: true,
            status: StatusSection::plain("Ready"),
            task: Some(TaskSection::new("Refreshing...", 0)),
            keys: vec![
                KeyChip::new("enter", "Open", StatusBarAction::key(KeyCode::Enter)),
                KeyChip::new("q", "Quit", StatusBarAction::key(KeyCode::Char('q'))),
            ],
            mode_indicators: vec![ModeIndicator::new("□", "zoom", StatusBarAction::key(KeyCode::Char('l')))],
        });

        // Mode indicators survive even when keys are shed
        assert_eq!(model.mode_indicators.len(), 1);
        assert!(model.visible_keys.len() < 2, "at least one key should have been shed");
    }
}
