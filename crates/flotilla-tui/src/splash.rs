use std::time::Duration;

use color_eyre::Result;
use ratatui_image::{
    picker::{cap_parser::QueryStdioOptions, Picker},
    StatefulImage,
};

pub async fn show_splash(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let img_bytes = include_bytes!("../../../assets/splash.webp");
    let dyn_img = image::load_from_memory(img_bytes).map_err(|e| color_eyre::eyre::eyre!("splash image: {e}"))?;
    let img_w = dyn_img.width() as f64;
    let img_h = dyn_img.height() as f64;

    let query_timeout_ms = std::env::var("FLOTILLA_SPLASH_QUERY_TIMEOUT_MS").ok().and_then(|raw| raw.parse::<u64>().ok()).unwrap_or(120);
    let mut query_options = QueryStdioOptions::default();
    query_options.timeout = Duration::from_millis(query_timeout_ms);
    let query_started = std::time::Instant::now();
    let picker = Picker::from_query_stdio_with_options(query_options).unwrap_or_else(|_| Picker::halfblocks());
    tracing::info!(
        query_ms = query_started.elapsed().as_millis() as u64,
        protocol = ?picker.protocol_type(),
        font_size = ?picker.font_size(),
        "splash image protocol picked"
    );

    let mut protocol = picker.new_resize_protocol(dyn_img);

    // Drain stale terminal responses left by Picker::from_query_stdio()
    while crossterm::event::poll(Duration::from_millis(10))? {
        let _ = crossterm::event::read()?;
    }

    // Guarantee a minimum visible time after first render (not just after splash setup).
    let min_visible = Duration::from_millis(700);
    let draw_started = std::time::Instant::now();
    let font_size = picker.font_size();
    let pixel_protocol = !matches!(picker.protocol_type(), ratatui_image::picker::ProtocolType::Halfblocks);
    terminal.draw(|f| {
        use ratatui::layout::{Constraint, Flex, Layout};
        let area = f.area();
        let mut scale = (area.width as f64 / img_w).min(area.height as f64 * 2.0 / img_h);
        if pixel_protocol {
            // Pixel protocols transmit uncompressed RGBA at the rendered cell
            // area's pixel size — upscaling past the image's native resolution
            // multiplies the payload for no visual gain (especially through a
            // multiplexer that has to re-parse every byte). Cap the rendered
            // area so the transmitted image never exceeds native pixels.
            scale = scale.min(1.0 / font_size.0 as f64).min(2.0 / font_size.1 as f64);
        }
        let rw = (img_w * scale) as u16;
        let rh = (img_h * scale / 2.0) as u16;
        let [area] = Layout::horizontal([Constraint::Length(rw.min(area.width))]).flex(Flex::Center).areas(area);
        let [area] = Layout::vertical([Constraint::Length(rh.min(area.height))]).flex(Flex::Center).areas(area);
        let widget = StatefulImage::default();
        f.render_stateful_widget(widget, area, &mut protocol);
    })?;
    tracing::info!(draw_ms = draw_started.elapsed().as_millis() as u64, "splash rendered");

    tokio::time::sleep(min_visible).await;

    // Drop any queued startup input (e.g. launch Enter key) so it doesn't
    // trigger immediate actions in the main event loop.
    while crossterm::event::poll(Duration::from_millis(0))? {
        let _ = crossterm::event::read()?;
    }
    Ok(())
}
