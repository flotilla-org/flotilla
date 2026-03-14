use std::{io::stdout, time::Duration};

use color_eyre::Result;
use crossterm::{
    event::{EnableMouseCapture, KeyCode, MouseButton, MouseEventKind},
    execute,
};

use crate::{
    app::{self, App, TabId, UiMode},
    event::{self, Event},
    event_log::LevelExt,
    ui,
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
    let replay_events = app.daemon.replay_since(&std::collections::HashMap::new()).await.unwrap_or_default();
    for event in replay_events {
        app.handle_daemon_event(event);
    }

    execute!(stdout(), EnableMouseCapture)?;
    let mut events = event::EventHandler::new(Duration::from_millis(50));
    events.attach_daemon(daemon_rx);

    // Initial draw before entering the event loop
    terminal.draw(|f| ui::render(&app.model, &mut app.ui, &app.in_flight, f))?;

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
                    let is_normal = matches!(app.ui.mode, UiMode::Normal);
                    if k.code == KeyCode::Char('r') && is_normal {
                        let repo = app.model.active_repo_root().clone();
                        let daemon = app.daemon.clone();
                        tokio::spawn(async move {
                            let _ = daemon.refresh(&repo).await;
                        });
                    } else {
                        app.handle_key(k);
                    }
                }
                Event::Mouse(m) => {
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) => {
                            let x = m.column;
                            let y = m.row;
                            let mut tab_clicked = false;

                            // Check event log filter area (click cycles level)
                            let ef = app.ui.layout.event_log_filter_area;
                            if x >= ef.x && x < ef.x + ef.width && y >= ef.y && y < ef.y + ef.height {
                                app.ui.event_log.filter = app.ui.event_log.filter.cycle();
                                app.ui.event_log.count = 0;
                                tab_clicked = true;
                            }

                            // Check which tab area was clicked
                            if !tab_clicked {
                                let hit = app
                                    .ui
                                    .layout
                                    .tab_areas
                                    .iter()
                                    .find(|(_, r)| x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height)
                                    .map(|(id, _)| id.clone());

                                match hit {
                                    Some(TabId::Flotilla) => {
                                        app.ui.mode = UiMode::Config;
                                        app.ui.drag.dragging_tab = None;
                                        tab_clicked = true;
                                    }
                                    Some(TabId::Repo(i)) => {
                                        app.switch_tab(i);
                                        app.ui.drag.dragging_tab = Some(i);
                                        app.ui.drag.start_x = x;
                                        app.ui.drag.active = false;
                                        tab_clicked = true;
                                    }
                                    Some(TabId::Gear) if !app.ui.mode.is_config() => {
                                        let sp = app.active_ui().show_providers;
                                        app.active_ui_mut().show_providers = !sp;
                                        tab_clicked = true;
                                    }
                                    Some(TabId::Add) => {
                                        app.open_file_picker_from_active_repo_parent();
                                        tab_clicked = true;
                                    }
                                    _ => {}
                                }
                            }
                            if !tab_clicked {
                                app.ui.drag.dragging_tab = None;
                                app.handle_mouse(m);
                            }
                        }
                        MouseEventKind::Drag(MouseButton::Left) => {
                            if let Some(dragging_idx) = app.ui.drag.dragging_tab {
                                if !app.ui.drag.active {
                                    let dx = (m.column as i16 - app.ui.drag.start_x as i16).unsigned_abs();
                                    if dx >= 2 {
                                        app.ui.drag.active = true;
                                    }
                                }
                                if app.ui.drag.active {
                                    for (id, r) in &app.ui.layout.tab_areas {
                                        if let TabId::Repo(i) = *id {
                                            if m.column >= r.x
                                                && m.column < r.x + r.width
                                                && m.row >= r.y
                                                && m.row < r.y + r.height
                                                && i != dragging_idx
                                            {
                                                app.model.repo_order.swap(dragging_idx, i);
                                                app.model.active_repo = i;
                                                app.ui.drag.dragging_tab = Some(i);
                                                break;
                                            }
                                        }
                                    }
                                }
                            } else {
                                app.handle_mouse(m);
                            }
                        }
                        MouseEventKind::Up(MouseButton::Left) => {
                            if app.ui.drag.dragging_tab.take().is_some() {
                                if app.ui.drag.active {
                                    app.config.save_tab_order(&app.model.repo_order);
                                }
                                app.ui.drag.active = false;
                            }
                        }
                        _ => {
                            app.handle_mouse(m);
                        }
                    }
                }
                Event::Tick => {} // already filtered out
                Event::Signal => {
                    app.should_quit = true;
                }
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
        terminal.draw(|f| ui::render(&app.model, &mut app.ui, &app.in_flight, f))?;

        if app.should_quit {
            break;
        }
    }

    crate::terminal::restore_terminal();
    Ok(())
}
