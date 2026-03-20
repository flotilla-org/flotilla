use std::{io::stdout, time::Duration};

use color_eyre::Result;
use crossterm::{
    event::{EnableMouseCapture, KeyCode, KeyModifiers, MouseEventKind},
    execute,
};

use crate::{
    app::{self, App},
    event::{self, Event},
    widgets::InteractiveWidget,
};

/// Run the TUI event loop: replay initial state, then process events until quit.
///
/// Takes ownership of a fully-constructed `App` (with daemon already connected)
/// and the ratatui terminal.  On return the terminal is restored.
pub async fn run_event_loop(mut terminal: ratatui::DefaultTerminal, mut app: App) -> Result<()> {
    // Subscribe before replay so events emitted between replay and the event
    // loop are buffered rather than silently dropped.
    let daemon_rx = app.daemon.subscribe();

    // Get initial state via replay_since (works for both in-process and socket).
    let replay_events =
        app.daemon.replay_since(&std::collections::HashMap::<flotilla_protocol::StreamKey, u64>::new()).await.unwrap_or_default();
    for event in replay_events {
        app.handle_daemon_event(event);
    }

    execute!(stdout(), EnableMouseCapture)?;
    let mut events = event::EventHandler::new(Duration::from_millis(50));
    events.attach_daemon(daemon_rx);

    // Initial draw before entering the event loop
    render_frame(&mut terminal, &mut app)?;

    loop {
        // ── Wait for the first event (blocking) ──
        let first = match events.next().await {
            Some(evt) => evt,
            None => break,
        };

        // ── Drain all pending events ──
        let mut batch = vec![first];
        while let Some(evt) = events.try_next() {
            batch.push(evt);
        }

        // ── Coalesce ──
        // Scroll: accumulate net delta. Ticks: discard.
        // Drags are NOT coalesced — each position triggers an adjacent-tab swap,
        // and the sequence must be preserved (including ordering relative to MouseUp).
        let mut scroll_delta: i32 = 0;
        let mut last_scroll_pos: Option<(u16, u16)> = None;
        let mut other_events: Vec<Event> = Vec::new();

        for evt in batch {
            match &evt {
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::ScrollDown => {
                        scroll_delta += 1;
                        last_scroll_pos = Some((m.column, m.row));
                    }
                    MouseEventKind::ScrollUp => {
                        scroll_delta -= 1;
                        last_scroll_pos = Some((m.column, m.row));
                    }
                    _ => other_events.push(evt),
                },
                Event::Tick => {
                    // Keep one tick to trigger a redraw when shimmer animation is active.
                    if app.needs_animation() && !other_events.iter().any(|e| matches!(e, Event::Tick)) {
                        other_events.push(evt);
                    }
                }
                _ => other_events.push(evt),
            }
        }

        // ── Process all non-coalesced events in order ──
        for evt in other_events {
            match evt {
                Event::Daemon(daemon_evt) => {
                    app.handle_daemon_event(*daemon_evt);
                }
                Event::Key(k) => {
                    // Ctrl-Z: suspend/resume (unix only)
                    #[cfg(unix)]
                    if k.code == KeyCode::Char('z') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        terminal = crate::terminal::suspend_and_resume();
                        continue;
                    }

                    app.handle_key(k);
                }
                Event::Mouse(m) => {
                    app.handle_mouse(m);
                }
                Event::Tick => {} // already filtered out
            }
        }

        // ── Apply coalesced scroll ──
        if scroll_delta != 0 {
            let (col, row) = last_scroll_pos.unwrap_or((0, 0));
            let abs = scroll_delta.unsigned_abs() as usize;
            let kind = if scroll_delta > 0 { MouseEventKind::ScrollDown } else { MouseEventKind::ScrollUp };
            let synthetic = crossterm::event::MouseEvent { kind, column: col, row, modifiers: crossterm::event::KeyModifiers::NONE };
            for _ in 0..abs {
                app.handle_mouse(synthetic);
            }
        }

        // ── Drain pending cancel ──
        if let Some(command_id) = app.pending_cancel.take() {
            let daemon = app.daemon.clone();
            tokio::spawn(async move {
                let _ = daemon.cancel(command_id).await;
            });
        }

        // ── Process queued commands ──
        while let Some((cmd, pending_ctx)) = app.proto_commands.take_next() {
            app::executor::dispatch(cmd, &mut app, pending_ctx).await;
        }

        // ── Draw once ──
        render_frame(&mut terminal, &mut app)?;

        if app.should_quit {
            break;
        }
    }

    crate::terminal::restore_terminal();
    Ok(())
}

/// Render one frame by calling `Screen::render()` which handles the base
/// layer and all modals.
fn render_frame(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let active_widget_mode = app.screen.active_mode_id();
    let active_widget_data = app.screen.active_status_data();
    terminal.draw(|f| {
        let area = f.area();
        let mut ctx = crate::widgets::RenderContext {
            model: &app.model,
            ui: &mut app.ui,
            theme: &app.theme,
            keymap: &app.keymap,
            in_flight: &app.in_flight,
            active_widget_mode,
            active_widget_data: active_widget_data.clone(),
        };
        app.screen.render(f, area, &mut ctx);
    })?;
    Ok(())
}
