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
    let pixel_font_size = (!matches!(picker.protocol_type(), ratatui_image::picker::ProtocolType::Halfblocks)).then(|| picker.font_size());
    terminal.draw(|f| {
        use ratatui::layout::{Constraint, Flex, Layout};
        let area = f.area();
        let scale = splash_scale(img_w, img_h, area.width, area.height, pixel_font_size);
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

/// Scale factor (in cells per image pixel, halfblock aspect: one cell ≈ 1×2
/// pixel units) fitting an `img_w`×`img_h` image into an `area_w`×`area_h`
/// cell area. With `pixel_font_size` set (pixel protocols), the scale is
/// additionally capped so the rendered cell area never exceeds the image's
/// native pixel size: pixel protocols transmit uncompressed RGBA at the
/// rendered area's pixel dimensions, so upscaling multiplies the payload for
/// no visual gain (especially through a multiplexer re-parsing every byte).
pub fn splash_scale(img_w: f64, img_h: f64, area_w: u16, area_h: u16, pixel_font_size: Option<(u16, u16)>) -> f64 {
    let scale = (area_w as f64 / img_w).min(area_h as f64 * 2.0 / img_h);
    match pixel_font_size {
        Some((font_w, font_h)) => scale.min(1.0 / font_w as f64).min(2.0 / font_h as f64),
        None => scale,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};
    use ratatui_image::{picker::Picker, StatefulImage};

    use super::splash_scale;

    const IMG: (f64, f64) = (1024.0, 559.0);

    fn rendered_px(scale: f64, font: (u16, u16)) -> (f64, f64) {
        let cells_w = (IMG.0 * scale).floor();
        let cells_h = (IMG.1 * scale / 2.0).floor();
        (cells_w * font.0 as f64, cells_h * font.1 as f64)
    }

    #[test]
    fn bundled_webp_decodes_and_renders_as_halfblocks() {
        let image = image::load_from_memory(include_bytes!("../../../assets/splash.webp")).expect("bundled WebP splash should decode");
        assert_eq!((image.width(), image.height()), (1024, 559));

        let mut protocol = Picker::halfblocks().new_resize_protocol(image);
        let mut terminal = Terminal::new(TestBackend::new(16, 8)).expect("test terminal");
        terminal
            .draw(|frame| frame.render_stateful_widget(StatefulImage::default(), frame.area(), &mut protocol))
            .expect("splash should render");

        assert!(
            terminal.backend().buffer().content().iter().any(|cell| matches!(cell.symbol(), "▀" | "▄")),
            "rendered splash should contain half-block pixels"
        );
    }

    #[test]
    fn pixel_protocol_never_transmits_above_native_resolution() {
        // Large retina-font terminal: uncapped fit would upscale well past native.
        let font = (16, 34);
        let scale = splash_scale(IMG.0, IMG.1, 238, 65, Some(font));
        let (px_w, px_h) = rendered_px(scale, font);
        assert!(px_w <= IMG.0, "width {px_w} exceeds native {}", IMG.0);
        assert!(px_h <= IMG.1, "height {px_h} exceeds native {}", IMG.1);
    }

    #[test]
    fn small_areas_still_fit_within_the_area() {
        let scale = splash_scale(IMG.0, IMG.1, 40, 12, Some((16, 34)));
        assert!((IMG.0 * scale) as u16 <= 40);
        assert!((IMG.1 * scale / 2.0) as u16 <= 12);
    }

    #[test]
    fn halfblocks_is_uncapped_and_fills_the_area() {
        // No pixel cost: the image should scale up to fill the fit dimension.
        let scale = splash_scale(IMG.0, IMG.1, 238, 65, None);
        let fills_width = (IMG.0 * scale).round() as u16 >= 237;
        let fills_height = (IMG.1 * scale / 2.0).round() as u16 >= 64;
        assert!(fills_width || fills_height, "uncapped fit should reach an area edge (scale {scale})");
    }

    #[test]
    fn cap_preserves_aspect_ratio() {
        let capped = splash_scale(IMG.0, IMG.1, 238, 65, Some((16, 34)));
        let uncapped = splash_scale(IMG.0, IMG.1, 238, 65, None);
        // A cap only shrinks the single shared scale factor, so aspect is
        // preserved by construction; sanity-check it actually shrank here.
        assert!(capped < uncapped);
    }
}
