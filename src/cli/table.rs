//! `flotilla table`: a described table rendered through binnacle (#4106). The rows come from the
//! daemon's `/tables/<name>/rows` over this host's socket, presented with the table's built-in
//! view file when it has one (`views/<kind>.<name>.json`) and binnacle's default view otherwise.
//! Piped, the table prints as aligned text, TSV or JSON; in a terminal it opens inline, live.
//! With no name, the daemon's described tables are listed the same way.

use std::{
    io::{self, Write},
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use binnacle_core::{Axis, ChangeNotice};
use binnacle_described::{decode_json_line, DescribedSource, Received, Scope};
use binnacle_described_http::{Blocking, Endpoint, Event, Options};
use binnacle_format::Timestamp;
use binnacle_layout::{IdMap, JsonId, Layout};
use binnacle_ratatui::cli::{print, run_inline, Exit, FlagArgs, Flags, InlineOptions, Live};
use color_eyre::{eyre::eyre, Result};
use flotilla_cli::cli::args::{Cli, TableArgs};
use serde_json::{json, Value as Json};

/// A view file baked into the binary: the presentation of one table.
struct BuiltinView {
    table: &'static str,
    name: &'static str,
    document: &'static str,
}

/// The built-in view files, by table. Read from a plain include until the baked data files
/// (#4198) land; a table without one presents with binnacle's default view.
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
    let presentation = |name: &str| {
        let view = views.iter().find(|(view, _)| *view == name).map(|(_, layout)| layout);
        (view.is_some() || name == "default").then(|| source.presentation(view, None))
    };
    let base = presentation("default").unwrap_or_default();
    let named: Vec<_> = views.iter().map(|(name, _)| (*name, presentation(name))).collect();
    let lookup = |name: &str| match name {
        "default" => Some(base.clone()),
        name => named.iter().find(|(view, _)| *view == name).and_then(|(_, layout)| layout.clone()),
    };
    flags.apply_to(source, &base, lookup).map_err(|error| eyre!("{error}"))
}

/// The binnacle flags `args` carries; `text` names binnacle's aligned format.
fn flags(args: &TableArgs) -> Result<Flags> {
    let format = args.format.clone().map(|format| if format == "text" { "aligned".into() } else { format });
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

pub(crate) async fn run_table_command(cli: &Cli, args: TableArgs) -> Result<()> {
    if let Some(remote) = cli.remote_daemon().map_err(|error| eyre!(error))? {
        return Err(eyre!(
            "`table` reads this host's daemon socket and cannot target {remote} yet; drop --daemon or unset FLOTILLA_DAEMON"
        ));
    }
    let flags = flags(&args)?;
    let socket = cli.socket_path();
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
        std::process::exit(status);
    }
    Ok(())
}

/// How many failed attempts in a row a print (not a terminal) retries before it gives up.
const PRINT_RETRIES: u32 = 4;
/// How long the first read may take before the command gives up.
const FIRST_READ: Duration = Duration::from_secs(30);
/// The longest one drain of the live feed takes, so frames still draw during a burst.
const BUDGET: Duration = Duration::from_millis(30);

/// Watches `table` over `socket` until its rows have come, then shows it.
fn show_live(socket: &Path, table: &str, namespace: &str, flags: &Flags, interactive: bool) -> Result<Exit> {
    if namespace.is_empty() {
        return Err(eyre!("--namespace can't be empty"));
    }
    // A print fails on a daemon that is down; a terminal keeps trying, saying so.
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
        drain(&mut source, &transport, &mut reconnecting).map_err(|error| eyre!("table {table}: {error}"))?;
        if reconnecting.is_some() && reconnecting != said {
            eprintln!("flotilla table: {}", reconnecting.as_deref().unwrap_or_default());
        }
        if Instant::now() > deadline {
            return Err(eyre!("table {table}: no rows within {} s from {}", FIRST_READ.as_secs(), socket.display()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let failed = std::cell::RefCell::new(None);
    let reconnecting = std::cell::RefCell::new(reconnecting);
    let live: Live<'_, DescribedSource> = Box::new(|source: &mut DescribedSource| {
        drain(source, &transport, &mut reconnecting.borrow_mut()).map_err(|error| *failed.borrow_mut() = Some(error)).ok()
    });
    let exit = show(&mut source, table, flags, interactive, Some((live, &reconnecting)))?;
    if let Some(error) = failed.take() {
        return Err(eyre!("table {table}: {error}"));
    }
    Ok(exit)
}

/// Feeds `source` what the watch has brought, for up to [`BUDGET`]; keeps `reconnecting` saying
/// why the transport is retrying, while it is. Returns the Change Notices, or the daemon's
/// refusal.
fn drain(
    source: &mut DescribedSource,
    transport: &Blocking,
    reconnecting: &mut Option<String>,
) -> std::result::Result<Vec<(Axis, ChangeNotice)>, String> {
    let mut notices = Vec::new();
    let until = Instant::now() + BUDGET;
    while Instant::now() < until {
        let Some(event) = transport.try_recv() else {
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
            transport.open(request);
        }
    }
    Ok(notices)
}

/// The daemon's answer as one line: its status, then the JSON body's `message`, else the
/// body's first line.
fn refusal(status: u16, body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let message = serde_json::from_slice::<Json>(body)
        .ok()
        .and_then(|body| body["message"].as_str().map(str::to_owned))
        .or_else(|| text.lines().map(str::trim).find(|line| !line.is_empty()).map(str::to_owned))
        .unwrap_or_default();
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
    live: Option<(Live<'_, DescribedSource>, &std::cell::RefCell<Option<String>>)>,
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `GET /tables` over the daemon's socket.
async fn listing(socket: &Path) -> Result<Json> {
    let client = rigging_tls::client_builder()
        .unix_socket(socket)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|error| eyre!("build daemon HTTP client: {error}"))?;
    let response = client
        .get("http://flotilla.local/tables")
        .header("accept", "application/json")
        .send()
        .await
        .map_err(|error| eyre!("could not reach the daemon at {}: {error}", socket.display()))?;
    let status = response.status();
    let body = response.bytes().await.map_err(|error| eyre!("read daemon response ({status}): {error}"))?;
    if !status.is_success() {
        return Err(eyre!("daemon refused the table listing: {}", refusal(status.as_u16(), &body)));
    }
    serde_json::from_slice(&body).map_err(|error| eyre!("daemon table listing is not JSON: {error}"))
}

/// The table listing as a described table of its own: one row per table, with its kind and the
/// built-in views it has.
fn listing_source(listing: &Json) -> Result<DescribedSource> {
    let kinds = listing["kinds"].as_array().ok_or_else(|| eyre!("daemon table listing has no kinds: {listing}"))?;
    let mut rows: Vec<Json> = kinds
        .iter()
        .flat_map(|kind| {
            let tables = kind["tables"].as_array().cloned().unwrap_or_default();
            tables.into_iter().filter_map(|table| table.as_str().map(str::to_owned)).map(move |table| {
                let views: Vec<&str> = VIEWS.iter().filter(|view| view.table == table).map(|view| view.name).collect();
                json!({"table": table, "kind": kind["kind"], "views": (!views.is_empty()).then(|| views.join(","))})
            })
        })
        .collect();
    rows.sort_by(|a, b| a["table"].as_str().cmp(&b["table"].as_str()));
    rows.dedup_by(|a, b| a["table"] == b["table"]);
    let field = |name: &str, title: &str, roles: &[&str], required: bool| json!({"name": name, "title": title, "type": "string", "roles": roles, "constraints": {"required": required}});
    let messages = [
        json!({"type": "table", "table": "tables", "schema": {
            "fields": [field("table", "Table", &["key"], true), field("kind", "Kind", &[], true), field("views", "Views", &[], false)],
            "primaryKey": ["table"],
            "foreignKeys": [],
        }}),
        json!({"type": "rows", "rows": rows, "partial": false}),
        json!({"type": "synced"}),
    ];
    let mut source = DescribedSource::new("tables", Scope::new("flotilla"));
    let _ = source.poll_request();
    for message in messages {
        let message = decode_json_line(message.to_string().as_bytes()).map_err(|error| eyre!("table listing: {error}"))?;
        let _ = source.receive(message);
    }
    Ok(source)
}

#[cfg(test)]
mod tests;
