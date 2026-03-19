use std::{
    fs,
    time::{Duration, Instant},
};

use color_eyre::Result;
use ratatui_image::{
    picker::{cap_parser::QueryStdioOptions, Picker},
    StatefulImage,
};

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let mut terminal = ratatui::init();
    flotilla_tui::terminal::install_panic_hook();

    let result = show_splash_with_report(&mut terminal).await;

    flotilla_tui::terminal::restore_terminal();
    result
}

async fn show_splash_with_report(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let img_bytes = include_bytes!("../../../assets/splash.webp");
    let decode_started = Instant::now();
    let dyn_img = image::load_from_memory(img_bytes).map_err(|e| color_eyre::eyre::eyre!("splash image: {e}"))?;
    let decode_elapsed = decode_started.elapsed();
    let img_w = dyn_img.width() as f64;
    let img_h = dyn_img.height() as f64;

    let query_timeout_ms = std::env::var("FLOTILLA_SPLASH_QUERY_TIMEOUT_MS").ok().and_then(|raw| raw.parse::<u64>().ok()).unwrap_or(120);
    let mut query_options = QueryStdioOptions::default();
    query_options.timeout = Duration::from_millis(query_timeout_ms);

    let query_started = Instant::now();
    let picker = Picker::from_query_stdio_with_options(query_options).unwrap_or_else(|_| Picker::halfblocks());
    let query_elapsed = query_started.elapsed();
    let protocol_type = picker.protocol_type();

    let resize_started = Instant::now();
    let mut protocol = picker.new_resize_protocol(dyn_img);
    let resize_elapsed = resize_started.elapsed();

    while crossterm::event::poll(Duration::from_millis(10))? {
        let _ = crossterm::event::read()?;
    }

    let show_started = Instant::now();
    let min_visible = Duration::from_millis(700);
    terminal.draw(|f| {
        use ratatui::layout::{Constraint, Flex, Layout};
        let area = f.area();
        let scale = (area.width as f64 / img_w).min(area.height as f64 * 2.0 / img_h);
        let rw = (img_w * scale) as u16;
        let rh = (img_h * scale / 2.0) as u16;
        let [area] = Layout::horizontal([Constraint::Length(rw.min(area.width))]).flex(Flex::Center).areas(area);
        let [area] = Layout::vertical([Constraint::Length(rh.min(area.height))]).flex(Flex::Center).areas(area);
        let widget = StatefulImage::default();
        f.render_stateful_widget(widget, area, &mut protocol);
    })?;

    tokio::time::sleep(min_visible).await;
    let show_elapsed = show_started.elapsed();

    while crossterm::event::poll(Duration::from_millis(0))? {
        let _ = crossterm::event::read()?;
    }

    let report = format!(
        "splash-fixture protocol={protocol_type:?} decode_ms={} query_ms={} resize_ms={} show_ms={}",
        decode_elapsed.as_millis(),
        query_elapsed.as_millis(),
        resize_elapsed.as_millis(),
        show_elapsed.as_millis()
    );
    let report_path =
        std::env::var("FLOTILLA_SPLASH_FIXTURE_REPORT").unwrap_or_else(|_| "/tmp/flotilla-splash-fixture-report.txt".to_string());
    fs::write(&report_path, format!("{report}\n"))
        .map_err(|err| color_eyre::eyre::eyre!("write splash fixture report {}: {err}", report_path))?;
    println!("{report}");

    Ok(())
}
