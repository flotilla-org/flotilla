//! Installer readiness and fingerprint diagnostics from the daemon's typed query.
use std::{collections::BTreeMap, fmt::Write};

use flotilla_protocol::{CommandValue, PeerConnectionState};

/// The matching CLI/daemon handshake has already succeeded. Readiness additionally
/// requires the control plane to return a fleet response containing this host.
/// The `Ok` lines name peers the daemon refused.
pub fn check(result: Result<CommandValue, String>) -> Result<Vec<String>, String> {
    match result? {
        CommandValue::FleetHealth(response) if response.hosts.iter().any(|host| host.is_local) => Ok(response
            .hosts
            .iter()
            .filter_map(|host| match &host.link {
                PeerConnectionState::Rejected { reason } => Some(format!("{}: rejected: {reason}", host.host)),
                _ => None,
            })
            .collect()),
        CommandValue::Error { message } => Err(message),
        _ => Err("daemon did not return fleet health containing the local host".into()),
    }
}

/// Status is best effort: unavailable diagnostics do not invalidate local status.
pub fn spread(result: Result<CommandValue, String>) -> String {
    let response = match result {
        Ok(CommandValue::FleetHealth(response)) => response,
        Err(_) | Ok(CommandValue::Error { .. }) => return "fleet:   unavailable (daemon query failed)\n".into(),
        _ => return "fleet:   unavailable (invalid daemon response)\n".into(),
    };
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for host in &response.hosts {
        let fingerprint = host.protocol_fingerprint.as_deref().filter(|fingerprint| !fingerprint.is_empty()).unwrap_or("unknown");
        let name = format!("{}{}", host.host, if host.is_local { " (local)" } else { "" });
        groups.entry(fingerprint).or_default().push(name);
    }
    let mut output = "fleet wire generations:\n".to_string();
    // Unknown generations sort last, after every named fingerprint.
    let unknown = groups.remove("unknown");
    for (fingerprint, mut names) in groups.into_iter().chain(unknown.map(|names| ("unknown", names))) {
        names.sort();
        writeln!(output, "  {fingerprint}: {}", names.join(", ")).expect("write to string");
    }
    output
}

/// The newest `limit` roll records of one host, oldest first, from the installer's
/// append-only log (tab-separated: time, generation, replaced, flotilla, cleat, by,
/// reason). Lines that do not parse are skipped.
pub fn rolls(host: &str, log: &str, limit: usize) -> String {
    let records: Vec<[&str; 7]> = log.lines().filter_map(|line| line.split('\t').collect::<Vec<_>>().try_into().ok()).collect();
    let mut output = String::new();
    for [time, generation, replaced, flotilla, cleat, by, reason] in &records[records.len().saturating_sub(limit)..] {
        let time = time.get(..16).map_or(*time, |minute| minute).to_string() + "Z";
        let rollback = if reason.is_empty() { String::new() } else { format!("  ROLLBACK \"{reason}\"") };
        writeln!(
            output,
            "{host}  {time}  {}  replaced {}{rollback}  flotilla {}  cleat {}  by {by}",
            roll_label(generation),
            roll_label(replaced),
            flotilla.get(..8).unwrap_or(flotilla),
            cleat.get(..8).unwrap_or(cleat),
        )
        .expect("write to string");
    }
    output
}

/// `20261028T015132Z-r900-f…-c…` is shown as `r900`.
fn roll_label(generation: &str) -> &str {
    generation.split('-').nth(1).unwrap_or(generation)
}

#[cfg(test)]
mod tests {
    use flotilla_protocol::{
        FleetHealthResponse, FleetHostRow, FleetHostStaleness, FleetObservationAgreement, HostName, PeerConnectionState,
    };

    use super::*;

    fn host(name: &str, local: bool, fingerprint: Option<&str>) -> FleetHostRow {
        FleetHostRow::builder()
            .host(HostName::new(name))
            .is_local(local)
            .configured(true)
            .link(PeerConnectionState::Connected)
            .daemon_generation(format!("instance-{name}"))
            .maybe_protocol_fingerprint(fingerprint.map(str::to_string))
            .crew_count(0)
            .convoy_count(0)
            .staleness(FleetHostStaleness::Current)
            .observation_agreement(FleetObservationAgreement::Agree)
            .build()
    }

    fn response(hosts: Vec<FleetHostRow>) -> CommandValue {
        CommandValue::FleetHealth(Box::new(FleetHealthResponse { hosts, ..Default::default() }))
    }

    // Readiness depends on a local row, even among duplicates or remote
    // peers. Generate counts through eight and every local-row mask, with fixed
    // empty/single/max boundaries in every run.
    #[hegel::test]
    fn readiness_requires_a_local_host(tc: hegel::TestCase) {
        let generated = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
        for count in [0, 1, 8, generated] {
            for mask in 0..(1 << count) {
                let hosts = (0..count).map(|index| host("duplicate", mask & (1 << index) != 0, None)).collect();
                assert_eq!(check(Ok(response(hosts))).is_ok(), mask != 0);
            }
        }
        for result in [Err("transport failed".into()), Ok(CommandValue::Error { message: "query refused".into() }), Ok(CommandValue::Ok)] {
            assert!(check(result).is_err());
        }
    }

    // A converged fleet groups by its handshake fingerprint even though each
    // daemon has a different instance id. A mixed install has two wire groups.
    #[hegel::test]
    fn spread_groups_wire_generations_independently_of_instances(tc: hegel::TestCase) {
        // Generate fleet sizes and which hosts have rolled; pin empty, singleton,
        // and eight-host boundaries and both converged and mixed cases each run.
        let count = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
        for count in [0, 1, 8, count] {
            for rolled in 0..=count {
                let hosts = (0..count)
                    .map(|index| host(&format!("host-{index}"), index == 0, Some(if index < rolled { "new" } else { "old" })))
                    .collect();
                let output = spread(Ok(response(hosts)));
                let groups = usize::from(rolled > 0) + usize::from(rolled < count);
                assert_eq!(output.lines().count(), 1 + groups);
                for (fingerprint, range) in [("new", 0..rolled), ("old", rolled..count)] {
                    if range.is_empty() {
                        continue;
                    }
                    let names = range
                        .map(|index| format!("host-{index}{}", if index == 0 { " (local)" } else { "" }))
                        .collect::<Vec<_>>()
                        .join(", ");
                    assert!(output.contains(&format!("  {fingerprint}: {names}\n")), "{output}");
                }
            }
        }
    }

    #[test]
    fn check_names_rejected_peers() {
        let mut feta = host("feta", false, None);
        feta.link = PeerConnectionState::Rejected { reason: "peer feta has [primitives] on, this host has it off".into() };
        let refused = check(Ok(response(vec![host("kiwi", true, None), feta]))).expect("local row present");
        assert_eq!(refused, vec!["feta: rejected: peer feta has [primitives] on, this host has it off".to_string()]);
    }

    // Generation diagnostics are invariant under host ordering, preserve all
    // rows (including duplicates), sort names, and group absent/empty versions last.
    #[test]
    fn spread_is_sorted_and_preserves_rows() {
        let hosts = vec![
            host("feta", true, Some("111")),
            host("kiwi", false, Some("222")),
            host("mango", false, Some("111")),
            host("pear", false, None),
            host("apple", false, Some("")),
            host("kiwi", false, Some("222")),
            host("banana", false, Some("zzz")),
        ];
        let expected = "fleet wire generations:\n  111: feta (local), mango\n  222: kiwi, kiwi\n  zzz: banana\n  unknown: apple, pear\n";
        for shift in 0..hosts.len() {
            let mut rotated = hosts.clone();
            rotated.rotate_left(shift);
            assert_eq!(spread(Ok(response(rotated.clone()))), expected);
            rotated.reverse();
            assert_eq!(spread(Ok(response(rotated))), expected);
        }
        assert_eq!(spread(Ok(response(vec![]))), "fleet wire generations:\n");
        assert!(spread(Err("transport failed".into())).contains("daemon query failed"));
        assert!(spread(Ok(CommandValue::Error { message: "query failed".into() })).contains("daemon query failed"));
        assert!(spread(Ok(CommandValue::Ok)).contains("invalid daemon response"));
    }

    #[test]
    fn rolls_show_the_newest_three_with_rollbacks_marked() {
        let line = |time: &str, rev: u32, replaced: u32, reason: &str| {
            format!(
                "{time}\t20261028T000000Z-r{rev}-fabc-cdef\t20261028T000000Z-r{replaced}-fabc-cdef\t{}\t{}\trobert\t{reason}\n",
                "b5e684bb".repeat(5),
                "caa0458f".repeat(5)
            )
        };
        let log = [
            line("2026-10-27T01:00:00Z", 1, 0, ""),
            line("2026-10-28T01:00:00Z", 2, 1, ""),
            line("2026-10-29T01:00:00Z", 3, 2, "bad build"),
            line("2026-10-30T04:59:12Z", 4, 3, ""),
            "garbage\n".to_string(),
        ]
        .concat();
        let shown = rolls("udder", &log, 3);
        assert_eq!(
            shown,
            "udder  2026-10-28T01:00Z  r2  replaced r1  flotilla b5e684bb  cleat caa0458f  by robert\n\
             udder  2026-10-29T01:00Z  r3  replaced r2  ROLLBACK \"bad build\"  flotilla b5e684bb  cleat caa0458f  by robert\n\
             udder  2026-10-30T04:59Z  r4  replaced r3  flotilla b5e684bb  cleat caa0458f  by robert\n"
        );
        assert_eq!(rolls("udder", "", 3), "");
    }
}
