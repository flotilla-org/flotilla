//! How a destination in a vessel's reach set is met (#4299). The rule: a
//! destination that resolves, on the controlling host, only to globally
//! routable addresses is met natively; one that resolves to any private,
//! shared (CGNAT, tailnet), loopback or link-local address must be carried
//! from the controlling host, by whatever means the environment offers.
use std::{collections::BTreeSet, net::IpAddr, time::Duration};

use async_trait::async_trait;
use flotilla_protocol::reach::{PrivateReach, ReachDestination, ReachMeans, ReachRoute};

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

/// Resolves destinations on this host. A seam for tests: the system resolver
/// is the only production one.
#[async_trait]
pub trait ReachResolver: Send + Sync {
    async fn resolve(&self, destination: &ReachDestination) -> Resolution;
}

/// Looks nothing up: every destination is unresolved here, so placement
/// refuses none and provisioning resolves each where it is carried. The
/// admission default until the daemon sets [`SystemResolver`].
#[derive(Debug, Clone, Copy, Default)]
pub struct NoLookup;

#[async_trait]
impl ReachResolver for NoLookup {
    async fn resolve(&self, _destination: &ReachDestination) -> Resolution {
        Resolution::Unresolved("not looked up on this host".into())
    }
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

/// How an environment whose private destinations are met by `private` meets
/// each destination: public ones natively, private ones by `private`. The
/// first destination it cannot meet is named in the error.
pub async fn routes(
    resolver: &dyn ReachResolver,
    reach: &BTreeSet<ReachDestination>,
    private: PrivateReach,
) -> Result<Vec<PlannedRoute>, String> {
    let mut routes = Vec::with_capacity(reach.len());
    for destination in reach {
        let (met, carried_to) = match resolver.resolve(destination).await {
            Resolution::Public => (ReachMeans::Native, None),
            Resolution::Private(address) => match private.means() {
                // An exposure stands at the destination's own name: an address names none.
                Some(ReachMeans::Exposure) if is_address(destination) => {
                    return Err(format!("reach destination {destination} is a private address; only a named destination can be carried"))
                }
                Some(means) => (means, (means == ReachMeans::Exposure).then_some(address)),
                None => return Err(format!("reach destination {destination} resolves to a private address this environment cannot carry")),
            },
            Resolution::Unresolved(reason) => {
                return Err(format!("reach destination {destination} does not resolve on the controlling host: {reason}"))
            }
        };
        routes.push(PlannedRoute { route: ReachRoute { destination: destination.clone(), met }, carried_to });
    }
    Ok(routes)
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

    // #4299: public destinations are met natively; private ones by the
    // environment's means, an exposure carried to the address that was
    // classified; a private address cannot stand under a name, and a
    // destination that does not resolve, or that the environment cannot
    // carry, fails naming it.
    #[tokio::test]
    async fn routes_meet_public_natively_and_private_by_the_environment() {
        let private: IpAddr = "10.0.0.5".parse().unwrap();
        let resolver = Fixed(BTreeMap::from([
            ("github.com", Resolution::Public),
            ("forgejo.lab.test", Resolution::Private(private)),
            ("10.0.0.5", Resolution::Private(private)),
        ]));
        let reach = BTreeSet::from([at("github.com"), at("forgejo.lab.test")]);
        let exposed = routes(&resolver, &reach, PrivateReach::Exposure).await.unwrap();
        assert_eq!(
            exposed,
            vec![
                PlannedRoute {
                    route: ReachRoute { destination: at("forgejo.lab.test"), met: ReachMeans::Exposure },
                    carried_to: Some(private)
                },
                PlannedRoute { route: ReachRoute { destination: at("github.com"), met: ReachMeans::Native }, carried_to: None },
            ]
        );
        let native = routes(&resolver, &reach, PrivateReach::Native).await.unwrap();
        assert!(native.iter().all(|planned| planned.route.met == ReachMeans::Native && planned.carried_to.is_none()));
        let unmet = routes(&resolver, &reach, PrivateReach::Unmet).await.unwrap_err();
        assert!(unmet.contains("forgejo.lab.test:443"), "{unmet}");
        let address = routes(&resolver, &BTreeSet::from([at("10.0.0.5")]), PrivateReach::Exposure).await.unwrap_err();
        assert!(address.contains("10.0.0.5:443"), "{address}");
        let unresolved = routes(&resolver, &BTreeSet::from([at("elsewhere.test")]), PrivateReach::Exposure).await.unwrap_err();
        assert!(unresolved.contains("elsewhere.test:443") && unresolved.contains("unknown"), "{unresolved}");
        assert_eq!(NoLookup.resolve(&at("github.com")).await, Resolution::Unresolved("not looked up on this host".into()));
    }
}
