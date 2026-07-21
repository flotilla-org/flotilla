//! Measure image-protocol detection and splash render cost in the current
//! terminal. Run inside a zellij pane and directly in the host terminal to
//! compare:
//!
//! ```sh
//! cargo run -p flotilla-tui --example splash_probe [query_timeout_ms] [--uncapped]
//! ```
//!
//! By default the draw applies the same native-resolution cap as the real
//! splash (`flotilla_tui::splash::splash_scale`). Pass `--uncapped` to
//! transmit at the full fit-to-area size instead — useful for stressing a
//! multiplexer's ingest throughput with a deliberately large payload.

use std::time::{Duration, Instant};

use flotilla_tui::splash::splash_scale;
use ratatui_image::{
    picker::{cap_parser::QueryStdioOptions, Picker},
    StatefulImage,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let uncapped = args.iter().any(|arg| arg == "--uncapped");
    let timeout_ms = args.iter().find_map(|raw| raw.parse::<u64>().ok()).unwrap_or(2000);

    let img_bytes = include_bytes!("../../../assets/splash.webp");
    let t = Instant::now();
    let dyn_img = image::load_from_memory(img_bytes)?;
    let decode = t.elapsed();
    let (img_w, img_h) = (dyn_img.width() as f64, dyn_img.height() as f64);

    let mut terminal = ratatui::init();

    let mut query_options = QueryStdioOptions::default();
    query_options.timeout = Duration::from_millis(timeout_ms);
    let t = Instant::now();
    let picker_result = Picker::from_query_stdio_with_options(query_options);
    let query = t.elapsed();
    let (picker, query_err) = match picker_result {
        Ok(picker) => (picker, None),
        Err(err) => (Picker::halfblocks(), Some(err.to_string())),
    };
    let protocol_type = picker.protocol_type();
    let font_size = picker.font_size();

    let mut protocol = picker.new_resize_protocol(dyn_img);

    while crossterm::event::poll(Duration::from_millis(10))? {
        let _ = crossterm::event::read()?;
    }

    fn draw(
        terminal: &mut ratatui::DefaultTerminal,
        protocol: &mut ratatui_image::protocol::StatefulProtocol,
        (img_w, img_h): (f64, f64),
        pixel_font_size: Option<(u16, u16)>,
        rendered: &mut (u16, u16),
    ) -> std::io::Result<()> {
        terminal.draw(|f| {
            use ratatui::layout::{Constraint, Flex, Layout};
            let area = f.area();
            let scale = splash_scale(img_w, img_h, area.width, area.height, pixel_font_size);
            let rw = (img_w * scale) as u16;
            let rh = (img_h * scale / 2.0) as u16;
            *rendered = (rw.min(area.width), rh.min(area.height));
            let [area] = Layout::horizontal([Constraint::Length(rendered.0)]).flex(Flex::Center).areas(area);
            let [area] = Layout::vertical([Constraint::Length(rendered.1)]).flex(Flex::Center).areas(area);
            f.render_stateful_widget(StatefulImage::default(), area, protocol);
        })?;
        Ok(())
    }

    // Mirror the real splash: pixel protocols are capped at native resolution
    // unless --uncapped asks for the full fit-to-area payload.
    let pixel_font_size = (!uncapped && !matches!(protocol_type, ratatui_image::picker::ProtocolType::Halfblocks)).then_some(font_size);

    let mut rendered = (0u16, 0u16);

    // First draw: resize + encode + transmit image data.
    let t = Instant::now();
    draw(&mut terminal, &mut protocol, (img_w, img_h), pixel_font_size, &mut rendered)?;
    let first_draw = t.elapsed();

    std::thread::sleep(Duration::from_millis(400));

    // Second draw: protocol already encoded; measures placement/redraw only.
    let t = Instant::now();
    draw(&mut terminal, &mut protocol, (img_w, img_h), pixel_font_size, &mut rendered)?;
    let second_draw = t.elapsed();

    std::thread::sleep(Duration::from_millis(400));
    ratatui::restore();

    let cells = (rendered.0 as u32, rendered.1 as u32);
    let px = (cells.0 * font_size.0 as u32, cells.1 * font_size.1 as u32);
    let rgba_bytes = px.0 as u64 * px.1 as u64 * 4;
    println!("splash probe");
    println!(
        "  TERM={} ZELLIJ={} TERM_PROGRAM={}",
        std::env::var("TERM").unwrap_or_default(),
        std::env::var("ZELLIJ").is_ok(),
        std::env::var("TERM_PROGRAM").unwrap_or_default()
    );
    println!("  webp decode:        {decode:>10.2?}");
    println!(
        "  picker query:       {query:>10.2?} (timeout {timeout_ms}ms{})",
        query_err.map(|e| format!(", ERROR: {e}")).unwrap_or_default()
    );
    println!("  protocol picked:    {protocol_type:?}");
    println!("  font size:          {font_size:?}");
    println!("  native-res cap:     {}", if pixel_font_size.is_some() { "applied (as in production splash)" } else { "off" });
    println!(
        "  render area:        {}x{} cells = {}x{} px (~{} KiB raw rgba, ~{} KiB base64)",
        cells.0,
        cells.1,
        px.0,
        px.1,
        rgba_bytes / 1024,
        rgba_bytes * 4 / 3 / 1024
    );
    println!("  first draw (encode+transmit): {first_draw:>10.2?}");
    println!("  second draw (placement only): {second_draw:>10.2?}");
    Ok(())
}
