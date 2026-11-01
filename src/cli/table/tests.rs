//! `flotilla table` over recorded `/tables` responses: `tests/fixtures/tables/convoys.ndjson` is
//! the convoys rows stream and `convoys.json` its description, recorded from the daemon's router
//! (`GET /tables/convoys/rows?namespace=flotilla`) over six seeded convoys; `listing.json` is its
//! `GET /tables`.

use binnacle_described::Decoder;
use binnacle_ratatui::cli::{print, run_inline_with};
use ratatui::{
    backend::TestBackend,
    crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers},
    Terminal, TerminalOptions, Viewport,
};

use super::*;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!("{}/tests/fixtures/tables/{name}", env!("CARGO_MANIFEST_DIR"));
    std::fs::read(&path).unwrap_or_else(|error| panic!("{path}: {error}"))
}

/// The recorded convoys rows, fed to a source for `table` as its host would.
fn convoys(table: &str) -> DescribedSource {
    let mut source = DescribedSource::new(table, Scope::new("flotilla"));
    let _ = source.poll_request();
    let mut decoder = Decoder::new(binnacle_described::Encoding::Ndjson);
    decoder.feed(&fixture("convoys.ndjson"));
    while let Some(message) = decoder.next().or_else(|| decoder.finish()) {
        let _ = source.receive(message.expect("recorded message decodes"));
    }
    assert!(source.status().synced, "the recording ends synced");
    source
}

/// The binnacle flags `flotilla table <args>` parses to, the global `--json` included.
fn parsed(args: &[&str]) -> Flags {
    use clap::Parser;
    let cli = Cli::try_parse_from(["flotilla", "table"].into_iter().chain(args.iter().copied())).expect("table args parse");
    match &cli.command {
        Some(flotilla_cli::cli::args::SubCommand::Table(table)) => flags(table, cli.json).expect("flags"),
        _ => panic!("not the table command"),
    }
}

/// `source` printed as `table` with `args`.
fn printed(source: &mut DescribedSource, table: &str, args: &[&str]) -> String {
    let flags = parsed(args);
    let layout = layout(source, table, &flags).expect("layout");
    let (mut out, mut note) = (Vec::new(), Vec::new());
    print(source, &layout, flags.format.unwrap_or_default(), &mut out, &mut note, None).expect("print");
    assert!(note.is_empty(), "{}", String::from_utf8_lossy(&note));
    String::from_utf8(out).expect("utf-8")
}

/// The first cell of each line after the header.
fn names(text: &str) -> Vec<&str> {
    text.lines().skip(1).map(|line| line.split(['\t', ' ']).next().unwrap_or_default()).collect()
}

const NEWEST_FIRST: [&str; 6] = ["convoy-5f6a", "convoy-4e5b", "convoy-3d4c", "convoy-2c3d", "convoy-1b2e", "convoy-0a1f"];

#[test]
fn convoys_print_with_the_view_files_columns_and_sort() {
    let text = printed(&mut convoys("convoys"), "convoys", &["--format", "text"]);
    let header: Vec<&str> = text.lines().next().unwrap().split_whitespace().collect();
    assert_eq!(header, ["Name", "Phase", "Namespace", "Created"], "{text}");
    assert_eq!(names(&text), NEWEST_FIRST, "{text}");
    let failed = text.lines().find(|line| line.starts_with("convoy-3d4c")).unwrap();
    assert_eq!(failed.split_whitespace().collect::<Vec<_>>()[1..3], ["Failed", "flotilla"], "{text}");
    assert!(text.lines().find(|line| line.starts_with("convoy-5f6a")).unwrap().contains(" - "), "no phase yet: {text}");
    assert!(text.lines().all(|line| line == line.trim_end()), "no trailing spaces: {text}");

    let tsv = printed(&mut convoys("convoys"), "convoys", &["--format", "tsv"]);
    let lines: Vec<&str> = tsv.lines().collect();
    assert_eq!(lines[0], "metadata.name\tstatus.phase\tmetadata.namespace\tmetadata.creationTimestamp");
    assert_eq!(names(&tsv), NEWEST_FIRST, "{tsv}");
    let landed: Vec<&str> = lines.iter().find(|line| line.starts_with("convoy-1b2e")).unwrap().split('\t').collect();
    assert_eq!(landed[..3], ["convoy-1b2e", "Landed", "flotilla"]);
    assert!(landed[3].starts_with("20") && landed[3].ends_with('Z'), "raw RFC 3339: {landed:?}");

    let json: Json = serde_json::from_str(&printed(&mut convoys("convoys"), "convoys", &["--format", "json"])).expect("json");
    // The global `--json` asks for the same; an explicit `--format` wins.
    assert_eq!(
        printed(&mut convoys("convoys"), "convoys", &["--json"]),
        printed(&mut convoys("convoys"), "convoys", &["--format", "json"])
    );
    assert_eq!(printed(&mut convoys("convoys"), "convoys", &["--json", "--format", "tsv"]), tsv);
    let rows = json.as_array().expect("an array of rows");
    assert_eq!(rows.len(), 6);
    let fields: Vec<&String> = rows[0].as_object().unwrap().keys().collect();
    assert_eq!(fields.len(), 4, "{json}");
    assert_eq!(rows[0]["metadata.name"], "convoy-5f6a");
}

#[test]
fn flags_patch_the_view() {
    let text =
        printed(&mut convoys("convoys"), "convoys", &["--format", "tsv", "--sort", "metadata.name", "--where", "status.phase==Landed"]);
    assert_eq!(names(&text), ["convoy-1b2e"], "{text}");
    let text = printed(&mut convoys("convoys"), "convoys", &["--format", "tsv", "--columns", "metadata.name,metadata.labels"]);
    assert!(text.starts_with("metadata.name\tmetadata.labels\n"), "{text}");
    // The default view by name is the view file; a view the table lacks is an error.
    let text = printed(&mut convoys("convoys"), "convoys", &["--format", "tsv", "--view", "default"]);
    assert_eq!(names(&text), NEWEST_FIRST, "{text}");
    let flags = parsed(&["--view", "nosuch"]);
    let unknown = layout(&mut convoys("convoys"), "convoys", &flags).unwrap_err().to_string();
    assert!(unknown.contains("nosuch"), "{unknown}");
    // An unknown format is refused while parsing, naming the formats.
    use clap::Parser;
    let refused = Cli::try_parse_from(["flotilla", "table", "convoys", "--format", "csv"]).err().expect("csv refused").to_string();
    assert!(refused.contains("text") && refused.contains("jsonl"), "{refused}");
    assert!(show_live(Path::new("/nonexistent"), "convoys", "", &flags, false).unwrap_err().to_string().contains("--namespace"));
}

#[test]
fn a_table_with_no_view_file_prints_binnacles_default_view() {
    // The same rows under a table name with no view file: the nested columns hidden, the key
    // and state columns leading, the rest in the description's order, the rows as served.
    let text = printed(&mut convoys("vessels"), "vessels", &["--format", "tsv"]);
    assert_eq!(
        text.lines().next().unwrap(),
        "metadata.name\tstatus.phase\tmetadata.namespace\tmetadata.resourceVersion\tmetadata.creationTimestamp\tmetadata.deletionTimestamp"
    );
    let mut served = NEWEST_FIRST;
    served.sort();
    assert_eq!(names(&text), served, "{text}");
}

// Replaced by #4198, whose baked view files get their meaning check there: this test goes with
// the include in `VIEWS`.
#[test]
fn every_built_in_view_parses_and_names_only_columns_its_table_has() {
    for view in VIEWS {
        // Each view's table needs its recorded description, `tests/fixtures/tables/<table>.json`.
        let description: Json = serde_json::from_slice(&fixture(&format!("{}.json", view.table))).expect("description");
        let fields: Vec<&str> = description["fields"].as_array().unwrap().iter().filter_map(|field| field["name"].as_str()).collect();
        let layout = Layout::<JsonId, JsonId>::from_json(view.document.as_bytes()).expect("view file is a binnacle Layout");
        let mut named: Vec<&JsonId> = Vec::new();
        if let Some(spec) = &layout.view {
            named.extend(spec.rows.sort.iter().map(|sort| &sort.id));
            named.extend(spec.rows.filter.conditions.iter().map(|condition| &condition.id));
            named.extend(&spec.cols.hidden);
        }
        if let Some(order) = &layout.order {
            named.extend(order.cols.iter().flatten());
        }
        if let Some(pins) = &layout.pins {
            named.extend(pins.cols.leading.iter().chain(&pins.cols.trailing));
        }
        assert!(!named.is_empty(), "{}.{} names no columns", view.table, view.name);
        for id in named {
            let name = id.value().as_str().unwrap_or_else(|| panic!("{}.{}: column {id} is not a field name", view.table, view.name));
            assert!(fields.contains(&name), "{}.{} names {name}, which {} lacks", view.table, view.name, view.table);
        }
    }
}

#[test]
fn the_listing_prints_each_table_with_its_kind_and_views() {
    let listing: Json = serde_json::from_slice(&fixture("listing.json")).expect("listing");
    let mut source = listing_source(&listing).expect("listing source");
    assert!(listing_source(&serde_json::json!({"tables": []})).is_err(), "a listing without kinds is an error");
    let text = printed(&mut source, "", &["--format", "tsv"]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "table\tkind\tviews");
    assert!(lines.contains(&"convoys\tConvoy\tdefault"), "{text}");
    assert!(lines.iter().any(|line| line.starts_with("vessels\tVessel\t")), "{text}");
    let mut tables = names(&text);
    tables.dedup();
    assert_eq!(tables.len(), lines.len() - 1, "each table once");
    assert!(tables.windows(2).all(|pair| pair[0] < pair[1]), "by name: {tables:?}");
}

#[test]
fn drain_feeds_messages_reports_retries_and_stops_at_a_refusal() {
    let mut source = DescribedSource::new("convoys", Scope::new("flotilla"));
    let _ = source.poll_request();
    let mut decoder = Decoder::new(binnacle_described::Encoding::Ndjson);
    decoder.feed(&fixture("convoys.ndjson"));
    let mut reconnecting = None;
    let mut events = vec![Event::Retrying { attempt: 1, wait: Duration::from_millis(250), status: Some(503), body: b"busy".to_vec() }];
    drain(&mut source, || events.pop(), |_| {}, &mut reconnecting).unwrap();
    assert_eq!(reconnecting.as_deref(), Some("reconnecting (503 busy), retry 1 in 250 ms"));
    // The next message clears it.
    let mut messages = std::iter::from_fn(|| decoder.next()).map(|message| Event::Message(message.unwrap()));
    drain(&mut source, || messages.next(), |_| {}, &mut reconnecting).unwrap();
    assert!(source.status().synced && reconnecting.is_none());
    assert_eq!(source.row_count(), 6);

    let mut events = vec![Event::Failed { status: 404, body: br#"{"message":"unknown table"}"#.to_vec() }];
    let refused = drain(&mut source, || events.pop(), |_| {}, &mut reconnecting).unwrap_err();
    assert_eq!(refused, "404 unknown table");

    // A stream that ends asks for the watch again.
    let mut opened = Vec::new();
    let mut events = vec![Event::Ended];
    drain(&mut source, || events.pop(), |request| opened.push(request), &mut reconnecting).unwrap();
    assert_eq!(opened.len(), 1);
}

#[test]
fn a_refusal_reads_as_the_daemons_message() {
    assert_eq!(refusal(404, br#"{"code":"not_found","message":"unknown table"}"#), "404 unknown table");
    assert_eq!(refusal(502, b"\n bad gateway \nmore"), "502 bad gateway");
    assert_eq!(refusal(0, b"connection refused"), "connection refused");
}

fn key(code: KeyCode) -> Option<TermEvent> {
    Some(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

fn ctrl(code: KeyCode) -> Option<TermEvent> {
    Some(TermEvent::Key(KeyEvent::new(code, KeyModifiers::CONTROL)))
}

fn typed(text: &str) -> Vec<Option<TermEvent>> {
    text.chars().map(|c| key(KeyCode::Char(c))).collect()
}

/// The convoys table inline on a test terminal, driven by `events`: how it ended, what it chose
/// (as `flotilla table` prints it) and what stayed on the terminal.
fn inline(events: Vec<Option<TermEvent>>) -> (Exit, Vec<String>, String) {
    let mut source = convoys("convoys");
    let layout = layout(&mut source, "convoys", &flags(&TableArgs::default(), false).unwrap()).unwrap();
    let mut terminal = Terminal::with_options(TestBackend::new(120, 30), TerminalOptions { viewport: Viewport::Inline(12) }).unwrap();
    let options = InlineOptions { origin: Some(Timestamp::from_unix_secs(1_793_232_000)), ..InlineOptions::default() };
    let exit = run_inline_with(&mut terminal, events, &mut source, &layout, options).unwrap();
    let chosen = match &exit {
        Exit::Chosen(selection) => chosen(&source, &selection.rows),
        _ => Vec::new(),
    };
    let screen = terminal.backend().to_string();
    (exit, chosen, screen)
}

#[test]
fn inline_enter_chooses_the_focused_row_and_q_leaves_choosing_nothing() {
    let (exit, chosen, screen) = inline(vec![key(KeyCode::Enter)]);
    assert!(matches!(exit, Exit::Chosen(_)));
    assert_eq!(chosen, ["convoy-5f6a"], "the newest convoy is first");
    assert!(screen.contains("convoy-0a1f"), "the rows stay behind: {screen}");
    let (_, chosen, _) = inline(vec![key(KeyCode::Down), key(KeyCode::Down), key(KeyCode::Enter)]);
    assert_eq!(chosen, ["convoy-3d4c"]);
    let (exit, chosen, screen) = inline(vec![key(KeyCode::Char('q'))]);
    assert_eq!(exit, Exit::Left);
    assert!(chosen.is_empty());
    assert!(screen.contains("Created") && screen.contains("convoy-2c3d"), "{screen}");
}

/// The convoys in the order the rows left behind show them.
fn shown(screen: &str) -> Vec<&str> {
    screen
        .lines()
        .filter_map(|line| line.trim_start_matches('"').split_whitespace().next())
        .filter(|name| name.starts_with("convoy-"))
        .collect()
}

#[test]
fn inline_sorts_by_the_focused_column() {
    let (_, _, screen) = inline(vec![key(KeyCode::Char('q'))]);
    assert_eq!(shown(&screen), NEWEST_FIRST, "the view file's sort: {screen}");
    // Focus starts on Name: `s` sorts by it ascending, then descending.
    let (_, _, screen) = inline(vec![key(KeyCode::Char('s')), key(KeyCode::Char('q'))]);
    let mut ascending = NEWEST_FIRST;
    ascending.sort();
    assert_eq!(shown(&screen), ascending, "{screen}");
    let (_, chosen, screen) = inline(vec![key(KeyCode::Char('s')), key(KeyCode::Char('s')), key(KeyCode::Enter)]);
    ascending.reverse();
    assert_eq!(shown(&screen), ascending, "{screen}");
    assert_eq!(chosen, ["convoy-5f6a"], "focus stays on its row through the sort");
}

#[test]
fn inline_filters_the_rows_to_the_quick_filters_matches() {
    let mut events = vec![ctrl(KeyCode::Char('l'))];
    events.extend(typed("3d4c"));
    events.extend([key(KeyCode::Enter), key(KeyCode::Enter)]);
    let (exit, chosen, screen) = inline(events);
    assert_eq!(shown(&screen), ["convoy-3d4c"], "{screen}");
    assert_eq!(chosen, ["convoy-3d4c"], "{exit:?}: {screen}");
}
