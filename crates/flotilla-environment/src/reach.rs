//! How a provider instance would meet each destination in a vessel's reach set
//! (#4299). The instance answers on its own controlling host, from the ways
//! its module declares. Its default rule: a destination that resolves there
//! only to globally routable addresses is met by the guest's own egress; one
//! that resolves to any private, shared (CGNAT, tailnet), loopback or
//! link-local address is met by the first declared way that can carry it.
use std::{collections::BTreeSet, net::IpAddr, time::Duration};

use async_trait::async_trait;
use flotilla_protocol::reach::{ReachDestination, ReachMeans, ReachRoute, ReachWay};

/// How long one destination's name lookup may take.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

/// Where a destination resolves on the host that looks it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Every address is globally routable.
    Public,
    /// At least one address is not: the first such, which a carrier reaches.
    Private(IpAddr),
    /// The name did not resolve, with why.
    Unresolved(String),
}

/// Resolves destinations on the host a provider instance runs on. A seam for
/// tests and fake vendors: the system resolver is the only production one.
#[async_trait]
pub trait ReachResolver: Send + Sync {
    async fn resolve(&self, destination: &ReachDestination) -> Resolution;
}

/// The host's own name resolution.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemResolver;

#[async_trait]
impl ReachResolver for SystemResolver {
    async fn resolve(&self, destination: &ReachDestination) -> Resolution {
        let host = destination.host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(address) = host.parse::<IpAddr>() {
            return classify([address]);
        }
        match tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, destination.port))).await {
            Ok(Ok(addresses)) => classify(addresses.map(|address| address.ip())),
            Ok(Err(error)) => Resolution::Unresolved(error.to_string()),
            Err(_) => Resolution::Unresolved(format!("lookup took longer than {}s", LOOKUP_TIMEOUT.as_secs())),
        }
    }
}

/// Public when there is at least one address and all are globally routable.
pub fn classify(addresses: impl IntoIterator<Item = IpAddr>) -> Resolution {
    let mut any = false;
    for address in addresses {
        if !is_global(address) {
            return Resolution::Private(address);
        }
        any = true;
    }
    if any {
        Resolution::Public
    } else {
        Resolution::Unresolved("no addresses".into())
    }
}

/// Whether `address` is reachable from the public internet. Conservative: an
/// address in a range that is not plainly global unicast counts as private, so
/// a misjudgement costs an unneeded exposure rather than a guest that times out.
/// Not exhaustive: rare special-purpose ranges inside 2000::/3 (ORCHID,
/// benchmarking 2001:2::/48) still count as public.
pub fn is_global(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            !(v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_documentation()
                // Shared address space (RFC 6598): carrier NAT and tailnets.
                || (a == 100 && (64..128).contains(&b))
                // Benchmarking (RFC 2544).
                || (a == 198 && (18..20).contains(&b))
                // This network, multicast, reserved and broadcast.
                || a == 0
                || a >= 224)
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            let embedded_v4 = |high: u16, low: u16| IpAddr::V4(std::net::Ipv4Addr::from((u32::from(high) << 16) | u32::from(low)));
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_global(IpAddr::V4(v4));
            }
            // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) carry an IPv4 address.
            if segments[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                return is_global(embedded_v4(segments[6], segments[7]));
            }
            if segments[0] == 0x2002 {
                return is_global(embedded_v4(segments[1], segments[2]));
            }
            // Only global unicast (2000::/3) is public, less Teredo (2001::/32)
            // and documentation (2001:db8::/32).
            (segments[0] & 0xe000) == 0x2000 && !(segments[0] == 0x2001 && matches!(segments[1], 0 | 0x0db8))
        }
    }
}

/// Whether the destination names an address rather than a host.
pub fn is_address(destination: &ReachDestination) -> bool {
    destination.host.trim_start_matches('[').trim_end_matches(']').parse::<IpAddr>().is_ok()
}

/// A destination's route, and for a carried one the private address the
/// controlling host resolved it to: the carrier goes there, so what was
/// classified is what is reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRoute {
    pub route: ReachRoute,
    pub carried_to: Option<IpAddr>,
}

/// One destination and how a candidate would meet it, or why it cannot.
pub type ReachAnswer = (ReachDestination, Result<PlannedRoute, String>);

/// How an instance whose module declares `ways` would meet each destination.
/// A vessel on the host's own network, when that is the preferred way, meets
/// every destination there, so nothing is looked up.
pub async fn answer(resolver: &dyn ReachResolver, reach: &BTreeSet<ReachDestination>, ways: &[ReachWay]) -> Vec<ReachAnswer> {
    let met = |destination: &ReachDestination, met: ReachMeans, carried_to: Option<IpAddr>| PlannedRoute {
        route: ReachRoute { destination: destination.clone(), met },
        carried_to,
    };
    if ways.first() == Some(&ReachWay::HostNetwork) {
        return reach.iter().map(|destination| (destination.clone(), Ok(met(destination, ReachMeans::HostNetwork, None)))).collect();
    }
    // Lookups run together, so the slowest bounds the wait, not their sum.
    let resolutions = futures::future::join_all(reach.iter().map(|destination| resolver.resolve(destination))).await;
    reach
        .iter()
        .zip(resolutions)
        .map(|(destination, resolution)| {
            let answer = match resolution {
                Resolution::Public => Ok(met(destination, ReachMeans::Egress, None)),
                Resolution::Private(address) => ways
                    .iter()
                    .find_map(|way| match way {
                        ReachWay::HostNetwork => Some(met(destination, ReachMeans::HostNetwork, None)),
                        // An exposure stands at the destination's own name: an address names none.
                        ReachWay::OutwardExposure => {
                            (!is_address(destination)).then(|| met(destination, ReachMeans::OutwardExposure, Some(address)))
                        }
                    })
                    .ok_or_else(|| {
                        if ways.contains(&ReachWay::OutwardExposure) {
                            format!("{destination} is a private address; an outward exposure carries only a named destination")
                        } else {
                            format!("{destination} resolves to a private address and no declared way reaches it")
                        }
                    }),
                // A vessel on the host's own network resolves it as the host does.
                Resolution::Unresolved(_) if ways.contains(&ReachWay::HostNetwork) => Ok(met(destination, ReachMeans::HostNetwork, None)),
                // A carrier cannot tell whether an unresolved name needs
                // carrying; refusing it names the destination rather than
                // leaving a crew to time out inside a clone.
                Resolution::Unresolved(reason) => Err(format!("{destination} does not resolve on the controlling host: {reason}")),
            };
            (destination.clone(), answer)
        })
        .collect()
}

/// Each destination's route, or the first destination the instance cannot
/// meet, named.
pub async fn routes(
    resolver: &dyn ReachResolver,
    reach: &BTreeSet<ReachDestination>,
    ways: &[ReachWay],
) -> Result<Vec<PlannedRoute>, String> {
    answer(resolver, reach, ways)
        .await
        .into_iter()
        .map(|(_, answer)| answer.map_err(|reason| format!("reach destination {reason}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn private_shared_and_local_addresses_are_not_global() {
        for private in [
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.100.1.1",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fd7a::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "2001::1",
            "64:ff9b::10.0.0.1",
            "2002:0a00:0001::1",
        ] {
            assert!(!is_global(private.parse().unwrap()), "{private}");
        }
        for public in ["140.82.112.3", "100.128.0.1", "2606:4700::1111", "::ffff:8.8.8.8", "64:ff9b::8.8.8.8", "2002:0808:0808::1"] {
            assert!(is_global(public.parse().unwrap()), "{public}");
        }
        let mixed = ["140.82.112.3".parse().unwrap(), "10.0.0.1".parse().unwrap()];
        assert_eq!(classify(mixed), Resolution::Private("10.0.0.1".parse().unwrap()));
        assert!(matches!(classify([]), Resolution::Unresolved(_)));
    }

    struct Fixed(BTreeMap<&'static str, Resolution>);

    #[async_trait]
    impl ReachResolver for Fixed {
        async fn resolve(&self, destination: &ReachDestination) -> Resolution {
            self.0.get(destination.host.as_str()).cloned().unwrap_or_else(|| Resolution::Unresolved("unknown".into()))
        }
    }

    fn at(host: &str) -> ReachDestination {
        ReachDestination::new(host, 443).unwrap()
    }

    // #4299: a candidate answers per destination from its declared ways. Public
    // destinations are met by the guest's egress; private ones by the first
    // way that can carry them, an exposure to the address that was classified.
    // A private address cannot stand under a name, and a destination that
    // does not resolve, or that no declared way reaches, is answered with why.
    #[tokio::test]
    async fn a_candidate_answers_each_destination_from_its_declared_ways() {
        let private: IpAddr = "10.0.0.5".parse().unwrap();
        let resolver = Fixed(BTreeMap::from([
            ("github.com", Resolution::Public),
            ("forgejo.lab.test", Resolution::Private(private)),
            ("10.0.0.5", Resolution::Private(private)),
        ]));
        let reach = BTreeSet::from([at("github.com"), at("forgejo.lab.test")]);
        let exposed = routes(&resolver, &reach, &[ReachWay::OutwardExposure]).await.unwrap();
        assert_eq!(
            exposed,
            vec![
                PlannedRoute {
                    route: ReachRoute { destination: at("forgejo.lab.test"), met: ReachMeans::OutwardExposure },
                    carried_to: Some(private)
                },
                PlannedRoute { route: ReachRoute { destination: at("github.com"), met: ReachMeans::Egress }, carried_to: None },
            ]
        );
        let met = |answers: Vec<ReachAnswer>| {
            answers.into_iter().map(|(destination, answer)| (destination.host, answer.map(|planned| planned.route.met))).collect::<Vec<_>>()
        };
        // A module that prefers an exposure but shares the host network carries
        // a private address there.
        let both = BTreeSet::from([at("10.0.0.5"), at("elsewhere.test"), at("forgejo.lab.test")]);
        assert_eq!(
            met(answer(&resolver, &both, &[ReachWay::OutwardExposure, ReachWay::HostNetwork]).await),
            vec![
                ("10.0.0.5".into(), Ok(ReachMeans::HostNetwork)),
                ("elsewhere.test".into(), Ok(ReachMeans::HostNetwork)),
                ("forgejo.lab.test".into(), Ok(ReachMeans::OutwardExposure)),
            ]
        );
        // With no ways, only what the guest's egress reaches is met.
        let none = met(answer(&resolver, &both, &[]).await);
        assert!(none.iter().all(|(host, answer)| answer.as_ref().is_err_and(|reason| reason.contains(host.as_str()))), "{none:?}");
        assert!(matches!(&none[1].1, Err(reason) if reason.contains("unknown")), "{none:?}");
        let address = routes(&resolver, &BTreeSet::from([at("10.0.0.5")]), &[ReachWay::OutwardExposure]).await.unwrap_err();
        assert!(address.contains("10.0.0.5:443") && address.contains("named"), "{address}");
        // Preferring the host network, nothing is looked up.
        let on_host = met(answer(&NoAnswers, &both, &[ReachWay::HostNetwork]).await);
        assert!(on_host.iter().all(|(_, answer)| *answer == Ok(ReachMeans::HostNetwork)), "{on_host:?}");
    }

    struct NoAnswers;

    #[async_trait]
    impl ReachResolver for NoAnswers {
        async fn resolve(&self, destination: &ReachDestination) -> Resolution {
            panic!("{destination} looked up")
        }
    }
}
