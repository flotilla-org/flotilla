use color_eyre::Result;
use std::time::Duration;

use ratatui_image::{
    picker::{cap_parser::QueryStdioOptions, Picker},
    StatefulImage,
};

pub async fn show_splash(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let img_bytes = include_bytes!("../../../assets/splash.webp");
    let dyn_img = image::load_from_memory(img_bytes)
        .map_err(|e| color_eyre::eyre::eyre!("splash image: {e}"))?;
    let img_w = dyn_img.width() as f64;
    let img_h = dyn_img.height() as f64;

    let query_timeout_ms = std::env::var("FLOTILLA_SPLASH_QUERY_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(120);
    let mut query_options = QueryStdioOptions::default();
    query_options.timeout = Duration::from_millis(query_timeout_ms);
    let picker = Picker::from_query_stdio_with_options(query_options)
        .unwrap_or_else(|_| Picker::halfblocks());

    let mut protocol = picker.new_resize_protocol(dyn_img);

    // Drain stale terminal responses left by Picker::from_query_stdio()
    while crossterm::event::poll(Duration::from_millis(10))? {
        let _ = crossterm::event::read()?;
    }

    // Guarantee a minimum visible time after first render (not just after splash setup).
    let min_visible = Duration::from_millis(700);
    terminal.draw(|f| {
        use ratatui::layout::{Constraint, Flex, Layout};
        let area = f.area();
        let scale = (area.width as f64 / img_w).min(area.height as f64 * 2.0 / img_h);
        let rw = (img_w * scale) as u16;
        let rh = (img_h * scale / 2.0) as u16;
        let [area] = Layout::horizontal([Constraint::Length(rw.min(area.width))])
            .flex(Flex::Center)
            .areas(area);
        let [area] = Layout::vertical([Constraint::Length(rh.min(area.height))])
            .flex(Flex::Center)
            .areas(area);
        let widget = StatefulImage::default();
        f.render_stateful_widget(widget, area, &mut protocol);
    })?;

    tokio::time::sleep(min_visible).await;

    // Drop any queued startup input (e.g. launch Enter key) so it doesn't
    // trigger immediate actions in the main event loop.
    while crossterm::event::poll(Duration::from_millis(0))? {
        let _ = crossterm::event::read()?;
    }
    Ok(())
}
