use std::{
    io::stdout,
    time::{Duration, Instant},
};

use color_eyre::Result;
use crossterm::{
    event::{EnableMouseCapture, KeyCode, KeyModifiers, MouseEventKind},
    execute,
    terminal::SetTitle,
};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout},
    style::Style,
    text::{Line, Text},
    widgets::{Block, Borders, Clear, Paragraph},
};

use crate::{
    app::{self, App},
    event::{self, Event},
    widgets::InteractiveWidget,
};

pub enum EventLoopExit {
    Quit,
    DaemonDisconnected(Box<App>),
}

#[derive(Default)]
struct SubscriptionRetry {
    next_attempt: Option<Instant>,
    failures: u32,
}

impl SubscriptionRetry {
    fn ready(&self, now: Instant) -> bool {
        self.next_attempt.is_none_or(|next| now >= next)
    }

    /// Returns whether this is the first failure in the current streak.
    fn failed(&mut self, now: Instant) -> bool {
        let first = self.failures == 0;
        let delay = (1_u64 << self.failures.min(5)).min(30);
        self.next_attempt = Some(now + Duration::from_secs(delay));
        self.failures = self.failures.saturating_add(1);
        first
    }

    fn succeeded(&mut self) {
        *self = Self::default();
    }
}

/// Run the TUI event loop: replay initial state, then process events until quit.
///
/// Takes ownership of a fully-constructed `App` (with daemon already connected)
/// and the ratatui terminal.  On return the terminal is restored.
pub async fn run_event_loop(mut terminal: ratatui::DefaultTerminal, mut app: App) -> Result<EventLoopExit> {
    // Subscribe before replay so events emitted between replay and the event
    // loop are buffered rather than silently dropped.
    let daemon_rx = app.daemon.subscribe();
    let mut events = event::EventHandler::new(Duration::from_millis(50));
    events.attach_daemon(daemon_rx);

    // Get initial state via replay_since (works for both in-process and socket).
    let replay_events =
        app.daemon.replay_since(&std::collections::HashMap::<flotilla_protocol::StreamKey, u64>::new()).await.unwrap_or_default();
    for event in replay_events {
        app.handle_daemon_event(event);
    }
    spawn_fleet_health_refresh(&app, events.sender());
    let mut fleet_health_refresh_in_flight = true;

    // Subscribe the named queries the open Views consume — the tab set is
    // the subscription set (ADR 0013). The subscribe replay returns the
    // initial result sets.
    let mut subscription_retry = SubscriptionRetry::default();
    resync_subscriptions(&mut app, &mut subscription_retry, Instant::now()).await;

    execute!(stdout(), EnableMouseCapture)?;
    let mut terminal_title = None;
    sync_terminal_title(&app, &mut terminal_title)?;
    let mut next_fleet_health_refresh = Instant::now() + Duration::from_secs(5);

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
                    // Keep one tick for animation and periodic fleet-health refresh.
                    if (app.needs_animation() || (!fleet_health_refresh_in_flight && Instant::now() >= next_fleet_health_refresh))
                        && !other_events.iter().any(|e| matches!(e, Event::Tick))
                    {
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
                Event::DaemonDisconnected => {
                    crate::terminal::restore_terminal();
                    return Ok(EventLoopExit::DaemonDisconnected(Box::new(app)));
                }
                Event::CommandDispatchCompleted { session_id, result, pending_ctx } => {
                    app::executor::handle_dispatch_completion(session_id, result, pending_ctx, &mut app);
                }
                Event::AttachDispatchCompleted { session_id, result } => {
                    app::executor::handle_attach_dispatch_completion(session_id, result, &mut app);
                }
                Event::FleetHealthRefreshed(result) => {
                    fleet_health_refresh_in_flight = false;
                    match result {
                        Ok(flotilla_protocol::CommandValue::FleetHealth(health)) => app.model.fleet_health = *health,
                        Ok(flotilla_protocol::CommandValue::Error { message }) => {
                            tracing::debug!(%message, "fleet health refresh unavailable");
                        }
                        Ok(other) => tracing::debug!(?other, "fleet health refresh returned unexpected value"),
                        Err(error) => tracing::debug!(%error, "fleet health refresh failed"),
                    }
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
                Event::Tick => {
                    if !fleet_health_refresh_in_flight && Instant::now() >= next_fleet_health_refresh {
                        spawn_fleet_health_refresh(&app, events.sender());
                        fleet_health_refresh_in_flight = true;
                        next_fleet_health_refresh = Instant::now() + Duration::from_secs(5);
                    }
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
            app::executor::dispatch(cmd, &mut app, pending_ctx, events.sender());
        }
        if let Some(plan) = app.pending_attach_plan.take() {
            events.pause_terminal_input().await;
            let (next_terminal, result) = crate::terminal::run_temporary_attach(&plan);
            terminal = next_terminal;
            terminal_title = None;
            events.resume_terminal_input();
            if let Err(message) = result {
                app.set_status_message(Some(message));
            }
        }
        app.drain_background_updates();

        // ── Re-sync query subscriptions after tab-set changes ──
        if app.subscriptions_dirty {
            resync_subscriptions(&mut app, &mut subscription_retry, Instant::now()).await;
        }

        // ── Check quit before rendering ──
        if app.should_quit {
            break;
        }

        // ── Draw once ──
        sync_terminal_title(&app, &mut terminal_title)?;
        render_frame(&mut terminal, &mut app)?;
    }

    app.daemon.unsubscribe_queries(app.session_id).await;
    crate::terminal::restore_terminal();
    Ok(EventLoopExit::Quit)
}

fn spawn_fleet_health_refresh(app: &App, event_tx: tokio::sync::mpsc::UnboundedSender<Event>) {
    let daemon = app.daemon.clone();
    let session_id = app.session_id;
    let command = flotilla_protocol::Command {
        node_id: None,
        provisioning_target: None,
        context_repo: None,
        action: flotilla_protocol::CommandAction::QueryFleetHealth {},
    };
    tokio::spawn(async move {
        let result = daemon.execute_query(command, session_id).await;
        let _ = event_tx.send(Event::FleetHealthRefreshed(result));
    });
}

fn sync_terminal_title(app: &App, current: &mut Option<String>) -> Result<()> {
    let next =
        app.views.is_scoped().then(|| crate::widgets::tabs::tab_label(app.views.active(), &app.model, &app.namespaces).trim().to_string());
    if next != *current {
        if let Some(title) = &next {
            execute!(stdout(), SetTitle(format!("{title} — flotilla")))?;
        }
        *current = next;
    }
    Ok(())
}

/// Replace the daemon-side query subscription set with the union the open
/// Views consume, applying any replayed result sets for stale cursors.
async fn resync_subscriptions(app: &mut App, retry: &mut SubscriptionRetry, now: Instant) {
    if !app.subscriptions_dirty || !retry.ready(now) {
        return;
    }
    let cursors = app.query_cursors();
    app.subscriptions_dirty = false;
    match app.daemon.subscribe_queries(app.session_id, &cursors).await {
        Ok(events) => {
            retry.succeeded();
            for event in events {
                app.handle_daemon_event(event);
            }
        }
        Err(e) => {
            app.subscriptions_dirty = true;
            if retry.failed(now) {
                tracing::warn!(%e, "query subscription re-sync failed; retrying with backoff");
            }
        }
    }
}

/// Render one frame by calling `Screen::render()` which handles the base
/// layer and all modals.
fn render_frame(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    terminal.draw(|f| {
        let area = f.area();
        let mut ctx = crate::widgets::RenderContext {
            model: &app.model,
            views: &mut app.views,
            ui: &mut app.ui,
            theme: &app.theme,
            keymap: &app.keymap,
            in_flight: &app.in_flight,
            namespaces: &app.namespaces,
            query_tables: &app.query_tables,
        };
        app.screen.render(f, area, &mut ctx);
    })?;
    Ok(())
}

/// Draw the honest, minimal surface shown while the daemon is unavailable.
pub fn render_reconnect_frame(
    terminal: &mut ratatui::DefaultTerminal,
    attempt: usize,
    detail: Option<&str>,
    theme: &crate::theme::Theme,
) -> Result<()> {
    terminal.draw(|frame| {
        let area = frame.area();
        frame.render_widget(Clear, area);
        let popup = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Fill(1), Constraint::Length(7), Constraint::Fill(1)])
            .split(area)[1];
        let popup = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Fill(1), Constraint::Percentage(70), Constraint::Fill(1)])
            .split(popup)[1];
        let mut lines = vec![
            Line::raw(""),
            Line::styled(format!("Daemon disconnected — reconnecting (attempt {attempt})…"), Style::default().fg(theme.text).bold()),
        ];
        if let Some(detail) = detail {
            lines.push(Line::raw(""));
            lines.push(Line::styled(detail, Style::default().fg(theme.muted)));
        }
        frame.render_widget(
            Paragraph::new(Text::from(lines))
                .alignment(Alignment::Center)
                .block(Block::default().borders(Borders::ALL).border_style(theme.block_style())),
            popup,
        );
    })?;
    Ok(())
}

#[cfg(test)]
mod reconnect_tests {
    use std::{
        sync::{atomic::Ordering, Arc},
        time::{Duration, Instant},
    };

    use flotilla_protocol::{QueryId, ViewAddress};

    use super::{resync_subscriptions, SubscriptionRetry};
    use crate::{
        app::test_support::{stub_app_with_daemon, StubDaemon},
        table_view::{PendingRowContext, RowId, RowState},
    };

    #[tokio::test]
    async fn reconnect_clears_pending_rows_in_inactive_views_when_resubscription_fails() {
        let mut app = stub_app_with_daemon(Arc::new(StubDaemon::new()), vec![]);
        let address = ViewAddress::Convoys { namespace: "default".into(), scope: None };
        app.views.open_or_focus(address.clone());
        let row = PendingRowContext {
            address: address.clone(),
            panel: None,
            query: QueryId::Convoys { scope: None },
            row_id: RowId::new("convoy"),
        };
        app.views.begin_pending_row(&row, "old action".into()).expect("row should begin submitting");
        app.views.open_or_focus(ViewAddress::Overview);

        let daemon = Arc::new(StubDaemon::builder().subscribe_result(Err("subscription unavailable".into())).build());
        app.reconnect_daemon(daemon, vec![]);
        resync_subscriptions(&mut app, &mut SubscriptionRetry::default(), Instant::now()).await;

        assert!(app.subscriptions_dirty, "failed subscription should be retried");
        let view = app.views.iter().find(|view| view.address() == Some(&address)).expect("inactive view should remain open");
        assert!(view.table_state.row_state(&row.row_id).is_none());
        app.views.begin_pending_row(&row, "new action".into()).expect("stale pending row should no longer block actions");
        let view = app.views.iter().find(|view| view.address() == Some(&address)).expect("inactive view should remain open");
        assert!(matches!(view.table_state.row_state(&row.row_id), Some(RowState::Submitting { .. })));
    }

    #[tokio::test]
    async fn failed_subscriptions_back_off_and_recover_after_a_transient_failure() {
        let daemon = Arc::new(StubDaemon::builder().subscribe_result(Err("temporarily unavailable".into())).build());
        let mut app = stub_app_with_daemon(daemon.clone(), vec![]);
        let mut retry = SubscriptionRetry::default();
        let start = Instant::now();

        resync_subscriptions(&mut app, &mut retry, start).await;
        assert!(app.subscriptions_dirty);
        assert_eq!(daemon.subscribe_calls.load(Ordering::SeqCst), 1);
        assert_eq!(retry.next_attempt, Some(start + Duration::from_secs(1)));

        resync_subscriptions(&mut app, &mut retry, start + Duration::from_millis(999)).await;
        assert_eq!(daemon.subscribe_calls.load(Ordering::SeqCst), 1);

        resync_subscriptions(&mut app, &mut retry, start + Duration::from_secs(1)).await;
        assert_eq!(daemon.subscribe_calls.load(Ordering::SeqCst), 2);
        assert_eq!(retry.next_attempt, Some(start + Duration::from_secs(3)));

        *daemon.subscribe_result.lock().expect("subscribe result lock") = Ok(vec![]);
        resync_subscriptions(&mut app, &mut retry, start + Duration::from_secs(2)).await;
        assert_eq!(daemon.subscribe_calls.load(Ordering::SeqCst), 2);
        resync_subscriptions(&mut app, &mut retry, start + Duration::from_secs(3)).await;
        assert_eq!(daemon.subscribe_calls.load(Ordering::SeqCst), 3);
        assert!(!app.subscriptions_dirty);
        assert_eq!(retry.failures, 0);
        assert_eq!(retry.next_attempt, None);
    }

    #[test]
    fn repeated_failures_warn_once_per_streak_and_cap_the_delay() {
        let mut retry = SubscriptionRetry::default();
        let start = Instant::now();
        assert!(retry.failed(start));
        for attempt in 1..8 {
            assert!(!retry.failed(start + Duration::from_secs(attempt)));
        }
        assert_eq!(retry.next_attempt, Some(start + Duration::from_secs(7 + 30)));
        retry.succeeded();
        assert!(retry.failed(start + Duration::from_secs(38)));
    }
}
