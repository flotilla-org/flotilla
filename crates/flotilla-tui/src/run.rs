use std::{io::stdout, time::Duration};

use color_eyre::Result;
use crossterm::{
    event::{EnableMouseCapture, KeyCode, KeyModifiers, MouseButton, MouseEventKind},
    execute,
};

use crate::{
    app::{self, App, UiMode},
    event::{self, Event},
    widgets::tab_bar::TabBarAction,
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

                    let is_normal = matches!(app.ui.mode, UiMode::Normal) && !app.has_modal();
                    if k.code == KeyCode::Char('r') && is_normal {
                        let repo = app.model.active_repo_root().clone();
                        let daemon = app.daemon.clone();
                        tokio::spawn(async move {
                            let _ = daemon
                                .execute(flotilla_protocol::Command {
                                    host: None,
                                    context_repo: None,
                                    action: flotilla_protocol::CommandAction::Refresh {
                                        repo: Some(flotilla_protocol::RepoSelector::Path(repo)),
                                    },
                                })
                                .await;
                        });
                    } else {
                        app.handle_key(k);
                    }
                }
                Event::Mouse(m) => match m.kind {
                    MouseEventKind::Down(MouseButton::Left) => {
                        let x = m.column;
                        let y = m.row;
                        // Check event log filter click first (owned by EventLogWidget)
                        if app.event_log_widget.handle_click(x, y) {
                            continue;
                        }
                        let action = app.tab_bar.handle_click(x, y, app.ui.mode.is_config());
                        let tab_clicked = match action {
                            TabBarAction::CycleEventLogFilter => {
                                // Fallback: TabBar detected the filter area. Delegate
                                // to the widget (should not normally fire since we
                                // check the widget first, but kept for safety).
                                app.event_log_widget.handle_click(x, y);
                                true
                            }
                            TabBarAction::SwitchToConfig => {
                                app.dismiss_modals();
                                app.ui.mode = UiMode::Config;
                                app.ui.drag.dragging_tab = None;
                                true
                            }
                            TabBarAction::SwitchToRepo(i) => {
                                app.dismiss_modals();
                                app.switch_tab(i);
                                app.ui.drag.dragging_tab = Some(i);
                                app.ui.drag.start_x = x;
                                app.ui.drag.active = false;
                                true
                            }
                            TabBarAction::OpenFilePicker => {
                                app.open_file_picker_from_active_repo_parent();
                                true
                            }
                            TabBarAction::None => false,
                        };
                        if !tab_clicked {
                            app.ui.drag.dragging_tab = None;
                            app.handle_mouse(m);
                        }
                    }
                    MouseEventKind::Drag(MouseButton::Left) => {
                        if app.ui.drag.dragging_tab.is_some() {
                            app.tab_bar.handle_drag(
                                m.column,
                                m.row,
                                &mut app.ui.drag,
                                &mut app.model.repo_order,
                                &mut app.model.active_repo,
                            );
                        } else {
                            app.handle_mouse(m);
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if app.ui.drag.dragging_tab.take().is_some() {
                            if app.ui.drag.active {
                                app.config.save_tab_order(&app.persisted_tab_order_paths());
                            }
                            app.ui.drag.active = false;
                        }
                    }
                    _ => {
                        app.handle_mouse(m);
                    }
                },
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

/// Render one frame by iterating the widget stack.
///
/// Takes the widget stack out of `app` to avoid borrow conflicts between the
/// stack iteration and the mutable `RenderContext` (which borrows `app.ui`,
/// `app.tab_bar`, etc.). The stack is restored after rendering.
fn render_frame(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    let mut stack = std::mem::take(&mut app.widget_stack);
    let active_widget_mode = stack.last().map(|w| w.mode_id());
    terminal.draw(|f| {
        let area = f.area();
        let mut ctx = crate::widgets::RenderContext {
            model: &app.model,
            ui: &mut app.ui,
            theme: &app.theme,
            keymap: &app.keymap,
            in_flight: &app.in_flight,
            active_widget_mode,
            tab_bar: &mut app.tab_bar,
            status_bar_widget: &mut app.status_bar_widget,
            event_log_widget: &mut app.event_log_widget,
            preview_panel: &app.preview_panel,
        };
        for widget in &mut stack {
            widget.render(f, area, &mut ctx);
        }
    })?;
    app.widget_stack = stack;
    Ok(())
}
