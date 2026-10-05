//! Installer readiness and fingerprint diagnostics from the daemon's typed query.
use std::{collections::BTreeMap, fmt::Write};

use flotilla_protocol::CommandValue;

/// The matching CLI/daemon handshake has already succeeded. Readiness additionally
/// requires the control plane to return a fleet response containing this host.
pub fn check(result: Result<CommandValue, String>) -> Result<(), String> {
    match result? {
        CommandValue::FleetHealth(response) if response.hosts.iter().any(|host| host.is_local) => Ok(()),
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
}
