//! `flotilla table`: a described table rendered through binnacle (#4106). The rows come from the
//! daemon's `/tables/<name>/rows` over this host's socket, presented with the table's built-in
//! view file when it has one (`views/<kind>.<name>.json`) and binnacle's default view otherwise.
//! Piped, the table prints as aligned text, TSV or JSON; in a terminal it opens inline, live.
//! With no name, the daemon's described tables are listed the same way.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    io::{self, Write},
    path::Path,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime},
};

use binnacle_core::{Axis, ChangeNotice};
use binnacle_described::{
    DescribedSource, Field, FieldType, HostRequest, Message, Received, Row, RowsMessage, Schema, Scope, TableMessage, Wire,
};
use binnacle_described_http::{Blocking, Endpoint, Event, Options};
use binnacle_format::Timestamp;
use binnacle_layout::{IdMap, JsonId, Layout};
use binnacle_ratatui::cli::{print, run_inline, Exit, FlagArgs, Flags, InlineOptions, Live};
use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, TableArgs, TableFormat};
use serde_json::Value as Json;

use super::daemon::connect_daemon;
use super::daemon_http::refusal_message;

/// A view file baked into the binary: the presentation of one table.
struct BuiltinView {
    table: &'static str,
    name: &'static str,
    document: &'static str,
}

/// The built-in view files, by table; a table without one presents with binnacle's default view.
/// Replaced by #4198: this plain include goes once the view files are baked into the binary
/// with the verb files.
const VIEWS: &[BuiltinView] =
    &[BuiltinView { table: "convoys", name: "default", document: include_str!("../../views/convoy.default.json") }];

/// The table's built-in views, by name.
fn views(table: &str) -> Result<Vec<(&'static str, Layout<JsonId, JsonId>)>> {
    VIEWS
        .iter()
        .filter(|view| view.table == table)
        .map(|view| {
            Layout::from_json(view.document.as_bytes())
                .map(|layout| (view.name, layout))
                .map_err(|error| eyre!("built-in view {}.{} is malformed: {error}", view.table, view.name))
        })
        .collect()
}

/// How `source` presents: its built-in `default` view (or binnacle's generic one) laid over
/// binnacle's default Layout, then patched by the command-line flags. `--view` names a built-in
/// view of the table.
fn layout(source: &mut DescribedSource, table: &str, flags: &Flags) -> Result<Layout<JsonId, JsonId>> {
    let views = views(table)?;
    let generic = source.default_layout();
    // `DescribedSource::presentation` with no user Layout, without holding the source.
    let present = |name: &str| {
        let view = views.iter().find(|(view, _)| *view == name).map(|(_, layout)| layout.clone());
        match (view, name) {
            (Some(view), _) => Some(Layout::layered(generic.clone(), view)),
            (None, "default") => Some(generic.clone()),
            (None, _) => None,
        }
    };
    let base = present("default").unwrap_or_default();
    flags.apply_to(source, &base, present).map_err(|error| eyre!("{error}"))
}

/// The binnacle flags `args` carries; `text` names binnacle's aligned format, and the global
/// `--json` asks for `json` unless `--format` says otherwise.
fn flags(args: &TableArgs, json: bool) -> Result<Flags> {
    let format = args.format.or(json.then_some(TableFormat::Json)).map(|format| {
        match format {
            TableFormat::Text => "aligned",
            TableFormat::Tsv => "tsv",
            TableFormat::Json => "json",
            TableFormat::Jsonl => "jsonl",
        }
        .to_owned()
    });
    FlagArgs {
        columns: args.columns.clone(),
        sort: args.sort.clone(),
        wheres: args.wheres.clone(),
        view: args.view.clone(),
        wide: args.wide,
        format,
        interactive: args.interactive,
        no_interactive: args.no_interactive,
    }
    .flags()
    .map_err(|error| eyre!("{error}"))
}

/// The status an interrupted `flotilla table` exits with (128 and the signal), once the runtime
/// has shut down.
static EXIT_STATUS: OnceLock<i32> = OnceLock::new();

pub(crate) fn table_exit_status() -> Option<i32> {
    EXIT_STATUS.get().copied()
}

pub(crate) async fn run_table_command(cli: &Cli, args: TableArgs) -> Result<()> {
    // Remote daemons: #4241.
    cli.require_local_daemon("table")?;
    let flags = flags(&args, cli.json)?;
    if cfg!(not(unix)) {
        return Err(eyre!(NO_UNIX_SOCKET));
    }
    // Start the daemon if none is running, as other commands do; the rows come over its socket.
    drop(connect_daemon(cli).await?);
    let socket = cli.client_paths().map_err(|error| eyre!(error))?.socket_path;
    let interactive = flags.interactive.now();
    let exit = match args.name {
        None => {
            let listing = listing(&socket).await?;
            tokio::task::spawn_blocking(move || {
                let mut source = listing_source(&listing)?;
                show(&mut source, "", &flags, interactive, None)
            })
            .await??
        }
        Some(name) => {
            let namespace = args.namespace;
            tokio::task::spawn_blocking(move || show_live(&socket, &name, &namespace, &flags, interactive)).await??
        }
    };
    if let Some(status) = exit.status() {
        let _ = EXIT_STATUS.set(status);
    }
    Ok(())
}

/// How many failed attempts in a row a print (not a terminal) retries before it gives up.
const PRINT_RETRIES: u32 = 4;
/// How long the first read may take before the command gives up.
const FIRST_READ: Duration = Duration::from_secs(30);
/// The longest one drain of the live feed takes, so frames still draw during a burst.
const BUDGET: Duration = Duration::from_millis(30);
/// How long the first read waits for each next event before checking its deadline.
const FIRST_WAIT: Duration = Duration::from_millis(100);

/// Watches `table` over `socket` until its rows have come, then shows it.
fn show_live(socket: &Path, table: &str, namespace: &str, flags: &Flags, interactive: bool) -> Result<Exit> {
    if namespace.is_empty() {
        return Err(eyre!("--namespace can't be empty"));
    }
    // A print gives up after a few failed attempts on a daemon that is down; a terminal retries,
    // saying so, until it has rows. Both stop at [`FIRST_READ`] if no rows come.
    let options = Options::new(Endpoint::Unix(socket.to_path_buf()));
    let options = if interactive { options } else { options.retries(Some(PRINT_RETRIES)) };
    let transport = Blocking::new(options)?;
    let mut source = DescribedSource::new(table, Scope::new(namespace));
    let mut reconnecting = None;
    let deadline = Instant::now() + FIRST_READ;
    while !source.status().synced {
        if let Some(request) = source.poll_request() {
            transport.open(request);
        }
        let said = reconnecting.clone();
        // Wait for the first event, then take what else has come without waiting.
        let mut first = Some(FIRST_WAIT);
        let next = || match first.take() {
            Some(wait) => transport.recv_timeout(wait),
            None => transport.try_recv(),
        };
        drain(&mut source, next, |request| transport.open(request), &mut reconnecting).map_err(|error| eyre!("table {table}: {error}"))?;
        if reconnecting.is_some() && reconnecting != said {
            eprintln!("flotilla table: {}", reconnecting.as_deref().unwrap_or_default());
        }
        if Instant::now() > deadline {
            return Err(eyre!("table {table}: no rows within {} s from {}", FIRST_READ.as_secs(), socket.display()));
        }
    }
    let failed = RefCell::new(None);
    let reconnecting = RefCell::new(reconnecting);
    // A refusal mid-session ends it (the feed returning `None` leaves as Esc does), so no stale
    // table stays open; the refusal is reported once the terminal is restored.
    let live: Live<'_, DescribedSource> = Box::new(|source: &mut DescribedSource| {
        drain(source, || transport.try_recv(), |request| transport.open(request), &mut reconnecting.borrow_mut())
            .map_err(|error| *failed.borrow_mut() = Some(error))
            .ok()
    });
    let exit = show(&mut source, table, flags, interactive, Some((live, &reconnecting)))?;
    if let Some(error) = failed.take() {
        return Err(eyre!("table {table}: {error}"));
    }
    Ok(exit)
}

/// Feeds `source` the watch's events from `next`, for up to [`BUDGET`]; `open` serves the
/// requests the source makes (the watch again after it ended). Keeps `reconnecting` saying why
/// the transport is retrying, while it is. Returns the Change Notices, or the daemon's refusal.
fn drain(
    source: &mut DescribedSource,
    mut next: impl FnMut() -> Option<Event>,
    mut open: impl FnMut(HostRequest),
    reconnecting: &mut Option<String>,
) -> std::result::Result<Vec<(Axis, ChangeNotice)>, String> {
    let mut notices = Vec::new();
    let until = Instant::now() + BUDGET;
    while Instant::now() < until {
        let Some(event) = next() else {
            break;
        };
        match event {
            Event::Message(message) => {
                *reconnecting = None;
                if let Received::Applied(applied) = source.receive(message) {
                    notices.extend(applied);
                }
            }
            Event::Retrying { attempt, wait, status, body } => {
                let why = refusal(status.unwrap_or(0), &body);
                *reconnecting = Some(format!("reconnecting ({why}), retry {attempt} in {} ms", wait.as_millis()));
            }
            Event::Ended => {
                source.ended();
            }
            Event::Failed { status, body } => return Err(refusal(status, &body)),
            // A message that can't be decoded is lost until the next resync; the rows stay.
            _ => {}
        }
        if let Some(request) = source.poll_request() {
            open(request);
        }
    }
    Ok(notices)
}

/// The daemon's answer as one line: its status, then what the body says.
fn refusal(status: u16, body: &[u8]) -> String {
    let message = refusal_message(body);
    match status {
        0 => message,
        status => format!("{status} {message}").trim_end().to_owned(),
    }
}

/// Prints `source` as `flags` ask, or opens it inline; on Enter, prints the chosen rows' keys.
fn show(
    source: &mut DescribedSource,
    table: &str,
    flags: &Flags,
    interactive: bool,
    live: Option<(Live<'_, DescribedSource>, &RefCell<Option<String>>)>,
) -> Result<Exit> {
    let layout = layout(source, table, flags)?;
    if !interactive {
        let mut out = io::stdout().lock();
        let mut note = Vec::new();
        let printed = print(source, &layout, flags.format.unwrap_or_default(), &mut out, &mut note, None).and_then(|()| out.flush());
        for line in String::from_utf8_lossy(&note).lines() {
            eprintln!("flotilla table: {line}");
        }
        return match printed {
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(Exit::Left),
            other => other.map(|()| Exit::Left).map_err(Into::into),
        };
    }
    let (live, note) = match live {
        Some((live, reconnecting)) => (Some(live), Some(reconnecting)),
        None => (None, None),
    };
    let options = InlineOptions {
        live,
        note: note.map(|note| -> Box<dyn FnMut() -> Option<String> + '_> { Box::new(move || note.borrow().clone()) }),
        origin: Some(now()),
        ..InlineOptions::default()
    };
    let exit = run_inline(source, &layout, options)?;
    if let Exit::Chosen(selection) = &exit {
        let mut out = io::stdout().lock();
        for line in chosen(source, &selection.rows) {
            writeln!(out, "{line}")?;
        }
    }
    Ok(exit)
}

/// The chosen rows' keys, one line each: a single text key as itself, any other as its JSON.
fn chosen(source: &DescribedSource, rows: &[binnacle_core::Key]) -> Vec<String> {
    rows.iter()
        .filter_map(|&row| IdMap::row_id(source, row))
        .map(|id| match id.value() {
            Json::Array(items) if items.len() == 1 && items[0].is_string() => items[0].as_str().unwrap_or_default().to_owned(),
            _ => id.to_string(),
        })
        .collect()
}

fn now() -> Timestamp {
    let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH);
    Timestamp::from_unix_nanos(now.map_or(0, |since| since.as_nanos() as i128))
}

/// What `flotilla table` says on a platform that hosts no local daemon socket.
const NO_UNIX_SOCKET: &str = "`table` reads this host's daemon over its Unix socket, which this platform does not host";

/// `GET /tables` over the daemon's socket.
#[cfg(unix)]
async fn listing(socket: &Path) -> Result<Json> {
    super::daemon_http::request_json(socket, reqwest::Method::GET, "/tables", "the table listing").await
}

#[cfg(not(unix))]
async fn listing(_socket: &Path) -> Result<Json> {
    Err(eyre!(NO_UNIX_SOCKET))
}

/// The table listing as a described table of its own: one row per table, with its kind and the
/// built-in views it has.
fn listing_source(listing: &Json) -> Result<DescribedSource> {
    let kinds = listing["kinds"].as_array().ok_or_else(|| eyre!("daemon table listing has no kinds: {listing}"))?;
    // A table name routes to one source (`/tables/{table}`), so the daemon lists each once; keyed
    // by name, the rows come out sorted and a name listed twice keeps its first kind.
    let mut rows: BTreeMap<String, Row> = BTreeMap::new();
    for kind in kinds {
        let kind_name = kind["kind"].as_str().map_or(Wire::Null, |kind| Wire::Text(kind.to_owned()));
        for table in kind["tables"].as_array().into_iter().flatten().filter_map(Json::as_str) {
            let views: Vec<&str> = VIEWS.iter().filter(|view| view.table == table).map(|view| view.name).collect();
            let views = if views.is_empty() { Wire::Null } else { Wire::Text(views.join(",")) };
            rows.entry(table.to_owned()).or_insert_with(|| {
                Row::from([("table".into(), Wire::Text(table.to_owned())), ("kind".into(), kind_name.clone()), ("views".into(), views)])
            });
        }
    }
    let field = |name: &str, title: &str, roles: &[&str], required: bool| {
        let mut field = Field::new(name, FieldType::String);
        field.title = Some(title.into());
        field.roles = roles.iter().map(|role| (*role).to_owned()).collect();
        field.required = Some(required);
        field
    };
    let messages = [
        Message::Table(TableMessage {
            schema: Schema {
                fields: vec![
                    field("table", "Table", &["key"], true),
                    field("kind", "Kind", &[], true),
                    field("views", "Views", &[], false),
                ],
                primary_key: vec!["table".into()],
                foreign_keys: Vec::new(),
            },
            incarnation: None,
            table: Some("tables".into()),
            sequence: None,
        }),
        Message::Rows(RowsMessage { rows: rows.into_values().collect(), cursor: None, partial: false, sequence: None }),
        Message::Synced { sequence: None },
    ];
    let mut source = DescribedSource::new("tables", Scope::new("flotilla"));
    let _ = source.poll_request();
    for message in messages {
        if let Received::Unhandled(unhandled) = source.receive(message) {
            return Err(eyre!("table listing: the source did not take {unhandled:?}"));
        }
    }
    if !source.status().synced {
        return Err(eyre!("table listing: the source is not synced"));
    }
    Ok(source)
}

#[cfg(test)]
mod tests;
