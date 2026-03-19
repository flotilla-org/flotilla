use super::{
    ghostty_ffi::{self, GhosttyFormatterFormat, GhosttyFormatterTerminalOptions, TerminalHandle},
    ClientCapabilities, ColorLevel, VtEngine,
};

const DEFAULT_MAX_SCROLLBACK: usize = 10_000;

pub struct GhosttyVtEngine {
    terminal: TerminalHandle,
    cols: u16,
    rows: u16,
}

impl GhosttyVtEngine {
    pub fn new(cols: u16, rows: u16) -> Self {
        let terminal = TerminalHandle::new(cols, rows, DEFAULT_MAX_SCROLLBACK).expect("create ghostty terminal");
        Self { terminal, cols, rows }
    }
}

impl VtEngine for GhosttyVtEngine {
    fn feed(&mut self, bytes: &[u8]) -> Result<(), String> {
        self.terminal.feed(bytes);
        Ok(())
    }

    fn resize(&mut self, cols: u16, rows: u16) -> Result<(), String> {
        self.terminal.resize(cols, rows)?;
        self.cols = cols;
        self.rows = rows;
        Ok(())
    }

    fn supports_replay(&self) -> bool {
        true
    }

    fn replay_payload(&self, capabilities: &ClientCapabilities) -> Result<Option<Vec<u8>>, String> {
        let mut options = GhosttyFormatterTerminalOptions::init();
        options.emit = GhosttyFormatterFormat::Vt;
        options.extra.modes = true;
        options.extra.scrolling_region = true;
        options.extra.pwd = true;
        options.extra.keyboard = capabilities.kitty_keyboard;
        options.extra.screen.cursor = true;
        options.extra.screen.style = true;
        options.extra.screen.hyperlink = true;
        options.extra.screen.protection = true;
        options.extra.screen.kitty_keyboard = capabilities.kitty_keyboard;
        options.extra.screen.charsets = true;
        options.extra.palette = matches!(capabilities.color_level, ColorLevel::Ansi256 | ColorLevel::TrueColor);

        let payload = ghostty_ffi::format_terminal_alloc(self.terminal.raw(), options)?;
        Ok((!payload.is_empty()).then_some(payload))
    }

    fn size(&self) -> (u16, u16) {
        (self.cols, self.rows)
    }
}
