//! Deep-link addresses for Views (ADR 0013).
//!
//! A `ViewAddress` names a View — an instance of a view kind with typed
//! parameters — in a surface-agnostic, shell-friendly form. Addresses are a
//! quasi-external contract: Presentation Manager materialise recipes hold
//! copies of them. Once a kind ships, its name and positional parameter
//! order are frozen; evolution is additive only (new kinds, new optional
//! `?key=value` parameters).

use std::{collections::BTreeMap, fmt, str::FromStr};

use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, CONTROLS, NON_ALPHANUMERIC};
use serde::{de, Deserialize, Deserializer, Serialize, Serializer};

use crate::{QueryScope, RepoIdentity, RepositoryKey};

/// Optional URI scheme prefix, accepted and stripped on parse.
pub const SCHEME_PREFIX: &str = "flotilla://";

/// Characters percent-encoded inside a single address segment: the segment
/// separator, reserved address syntax, and characters a shell or renderer
/// would misread.
const SEGMENT: &AsciiSet = &CONTROLS.add(b'/').add(b'?').add(b'#').add(b'%').add(b' ');

/// RFC 3986 query-component values: preserve exactly the unreserved set.
const QUERY_COMPONENT: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

/// The address of a View: kind + typed parameters. Identity for
/// open-or-focus semantics and the persisted form in `open-views.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ViewAddress {
    /// The overview/home view: registered repos, hosts, config.
    Overview,
    /// All convoys in one namespace.
    Convoys { namespace: String },
    /// All terminal sessions that are not associated with a Convoy.
    Independents,
    /// One convoy: its vessel DAG, phases, and intents.
    Convoy { namespace: String, name: String },
    /// One vessel: crew, work state, attach.
    Vessel { namespace: String, convoy: String, vessel: String },
    /// A project dashboard: the work of one Project resource.
    Project { namespace: String, name: String },
    /// The demand-backed issues window for one Project.
    Issues { scope: QueryScope },
    /// Concrete checkouts fleet-wide or narrowed to one Project.
    Checkouts { scope: Option<QueryScope> },
    /// The per-repository view, identified by canonical remote identity.
    Repo {
        identity: RepoIdentity,
        /// Storage-safe Repository resource key used by scoped queries.
        repository_key: Option<RepositoryKey>,
    },
}

impl ViewAddress {
    pub fn repo(identity: RepoIdentity) -> Self {
        Self::Repo { identity, repository_key: None }
    }

    pub fn repo_with_key(identity: RepoIdentity, repository_key: RepositoryKey) -> Self {
        Self::Repo { identity, repository_key: Some(repository_key) }
    }

    /// The frozen kind token this address starts with.
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Convoys { .. } => "convoys",
            Self::Independents => "independents",
            Self::Convoy { .. } => "convoy",
            Self::Vessel { .. } => "vessel",
            Self::Project { .. } => "project",
            Self::Issues { .. } => "issues",
            Self::Checkouts { .. } => "checkouts",
            Self::Repo { .. } => "repo",
        }
    }
}

fn encode(segment: &str) -> impl fmt::Display + '_ {
    utf8_percent_encode(segment, SEGMENT)
}

fn encode_query_component(value: &str) -> impl fmt::Display + '_ {
    utf8_percent_encode(value, QUERY_COMPONENT)
}

fn decode(segment: &str) -> Result<String, String> {
    percent_decode_str(segment)
        .decode_utf8()
        .map(|s| s.into_owned())
        .map_err(|_| format!("invalid percent-encoding in address segment: {segment}"))
}

impl fmt::Display for ViewAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Overview => f.write_str("overview"),
            Self::Convoys { namespace } => write!(f, "convoys/{}", encode(namespace)),
            Self::Independents => f.write_str("independents"),
            Self::Convoy { namespace, name } => write!(f, "convoy/{}/{}", encode(namespace), encode(name)),
            Self::Vessel { namespace, convoy, vessel } => {
                write!(f, "vessel/{}/{}/{}", encode(namespace), encode(convoy), encode(vessel))
            }
            Self::Project { namespace, name } => write!(f, "project/{}/{}", encode(namespace), encode(name)),
            Self::Issues { scope } => {
                write!(f, "issues?project={}", encode_query_component(&format!("{}/{}", scope.namespace, scope.name)))
            }
            Self::Checkouts { scope: Some(scope) } => {
                write!(f, "checkouts?project={}", encode_query_component(&format!("{}/{}", scope.namespace, scope.name)))
            }
            Self::Checkouts { scope: None } => f.write_str("checkouts"),
            Self::Repo { identity, repository_key } => {
                write!(f, "repo/{}", encode(&identity.authority))?;
                if identity.path.split('/').any(|part| part.is_empty()) {
                    // Paths with empty components (e.g. the absolute-path
                    // fallback identity of a remote-less local repo) cannot
                    // render as pretty segments — encode the whole path as
                    // one segment so the address still round-trips.
                    write!(f, "/{}", encode(&identity.path))?;
                } else {
                    for part in identity.path.split('/') {
                        write!(f, "/{}", encode(part))?;
                    }
                }
                if let Some(key) = repository_key {
                    write!(f, "?key={}", encode(&key.0))?;
                }
                Ok(())
            }
        }
    }
}

impl FromStr for ViewAddress {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.strip_prefix(SCHEME_PREFIX).unwrap_or(s);
        if s.is_empty() {
            return Err("empty view address".to_string());
        }
        if s.contains('#') {
            return Err(format!("view addresses do not support fragments: {s}"));
        }
        let (path, query) = s.split_once('?').map_or((s, None), |(path, query)| (path, Some(query)));
        let mut parameters = parse_parameters(query)?;
        let segments: Vec<&str> = path.split('/').collect();
        if segments.iter().any(|seg| seg.is_empty()) {
            return Err(format!("view address has an empty segment: {s}"));
        }
        let kind = segments[0];
        let address = match kind {
            "overview" => match segments.len() {
                1 => Ok(Self::Overview),
                _ => Err(format!("overview takes no parameters: {s}")),
            },
            "convoys" => match segments[1..] {
                [namespace] => Ok(Self::Convoys { namespace: decode(namespace)? }),
                _ => Err(format!("convoys takes exactly one parameter (namespace): {s}")),
            },
            "independents" => match segments.len() {
                1 => Ok(Self::Independents),
                _ => Err(format!("independents takes no parameters: {s}")),
            },
            "convoy" => match segments[1..] {
                [namespace, name] => Ok(Self::Convoy { namespace: decode(namespace)?, name: decode(name)? }),
                _ => Err(format!("convoy takes exactly two parameters (namespace, name): {s}")),
            },
            "vessel" => match segments[1..] {
                [namespace, convoy, vessel] => {
                    Ok(Self::Vessel { namespace: decode(namespace)?, convoy: decode(convoy)?, vessel: decode(vessel)? })
                }
                _ => Err(format!("vessel takes exactly three parameters (namespace, convoy, vessel): {s}")),
            },
            "project" => match segments[1..] {
                [namespace, name] => Ok(Self::Project { namespace: decode(namespace)?, name: decode(name)? }),
                _ => Err(format!("project takes exactly two parameters (namespace, name): {s}")),
            },
            kind if kind.eq_ignore_ascii_case("issues") => match segments.len() {
                1 => {
                    let value = parameters.remove("project").ok_or_else(|| "issues requires a project parameter".to_string())?;
                    Ok(Self::Issues { scope: parse_project_scope(value)? })
                }
                _ => Err(format!("issues takes no positional parameters: {s}")),
            },
            kind if kind.eq_ignore_ascii_case("checkouts") => match segments.len() {
                1 => {
                    let scope = parameters.remove("project").map(parse_project_scope).transpose()?;
                    Ok(Self::Checkouts { scope })
                }
                _ => Err(format!("checkouts takes no positional parameters: {s}")),
            },
            "repo" => match &segments[1..] {
                [] | [_] => Err(format!("repo takes an authority and a path: {s}")),
                [authority, path @ ..] => {
                    let path = path.iter().map(|seg| decode(seg)).collect::<Result<Vec<_>, _>>()?.join("/");
                    let repository_key = parameters.remove("key").map(|value| decode(value).map(RepositoryKey)).transpose()?;
                    Ok(Self::Repo { identity: RepoIdentity { authority: decode(authority)?, path }, repository_key })
                }
            },
            kind => Err(format!("unknown view kind: {kind}")),
        }?;
        if let Some((key, _)) = parameters.first_key_value() {
            return Err(format!("unsupported parameter `{key}` for {} view", address.kind_name()));
        }
        Ok(address)
    }
}

fn parse_project_scope(value: &str) -> Result<QueryScope, String> {
    let decoded = decode(value)?;
    // Project resource namespace/name components cannot contain `/`, so the
    // single decoded separator is unambiguous.
    let Some((namespace, name)) = decoded.split_once('/') else {
        return Err(format!("project scope must be <namespace>/<name>: {decoded}"));
    };
    if namespace.is_empty() || name.is_empty() || name.contains('/') {
        return Err(format!("project scope must be <namespace>/<name>: {decoded}"));
    }
    Ok(QueryScope::new(namespace, name))
}

fn parse_parameters(query: Option<&str>) -> Result<BTreeMap<&str, &str>, String> {
    let Some(query) = query else { return Ok(BTreeMap::new()) };
    if query.is_empty() {
        return Err("view address has an empty parameter list".to_string());
    }
    let mut parameters = BTreeMap::new();
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').ok_or_else(|| format!("view parameter must be key=value: {pair}"))?;
        if key.is_empty() || value.is_empty() {
            return Err(format!("view parameter key and value must be non-empty: {pair}"));
        }
        if parameters.insert(key, value).is_some() {
            return Err(format!("duplicate view parameter: {key}"));
        }
    }
    Ok(parameters)
}

impl Serialize for ViewAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ViewAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(authority: &str, path: &str) -> ViewAddress {
        ViewAddress::Repo { identity: RepoIdentity { authority: authority.to_string(), path: path.to_string() }, repository_key: None }
    }

    #[test]
    fn overview_round_trips() {
        assert_eq!("overview".parse::<ViewAddress>().expect("parse"), ViewAddress::Overview);
        assert_eq!(ViewAddress::Overview.to_string(), "overview");
    }

    #[test]
    fn convoys_round_trips() {
        let addr = ViewAddress::Convoys { namespace: "flotilla".to_string() };
        assert_eq!(addr.to_string(), "convoys/flotilla");
        assert_eq!("convoys/flotilla".parse::<ViewAddress>().expect("parse"), addr);
    }

    #[test]
    fn independents_round_trips() {
        assert_eq!("independents".parse::<ViewAddress>().expect("parse"), ViewAddress::Independents);
        assert_eq!(ViewAddress::Independents.to_string(), "independents");
    }

    #[test]
    fn project_scoped_table_addresses_round_trip_canonically() {
        let issues = "issues?project=flotilla%2froadmap".parse::<ViewAddress>().expect("parse issues address");
        assert_eq!(issues.to_string(), "issues?project=flotilla%2Froadmap");

        let checkouts = "CHECKOUTS?project=flotilla%2froadmap".parse::<ViewAddress>().expect("parse checkouts address");
        assert_eq!(checkouts.to_string(), "checkouts?project=flotilla%2Froadmap");

        let reserved = ViewAddress::Issues { scope: QueryScope::new("flotilla+platform", "road map") };
        assert_eq!(reserved.to_string(), "issues?project=flotilla%2Bplatform%2Froad%20map");
        assert_eq!(reserved.to_string().parse::<ViewAddress>().expect("parse reserved characters"), reserved);
    }

    #[test]
    fn case_insensitive_family_parsing_is_limited_to_the_new_query_families() {
        assert_eq!(
            "ISSUES?project=flotilla%2Froadmap".parse::<ViewAddress>().expect("parse issues").to_string(),
            "issues?project=flotilla%2Froadmap"
        );
        assert!("OVERVIEW".parse::<ViewAddress>().is_err());
        assert!("REPO/github.com/flotilla-org/flotilla".parse::<ViewAddress>().is_err());
    }

    #[test]
    fn fleet_checkouts_is_valid_but_bare_issues_is_not() {
        let checkouts = "checkouts".parse::<ViewAddress>().expect("parse fleet checkouts");
        assert_eq!(checkouts.to_string(), "checkouts");

        assert_eq!("issues".parse::<ViewAddress>().expect_err("bare issues must fail"), "issues requires a project parameter");
    }

    #[test]
    fn repo_round_trips_with_multi_segment_path() {
        let addr = repo("github.com", "flotilla-org/flotilla");
        assert_eq!(addr.to_string(), "repo/github.com/flotilla-org/flotilla");
        assert_eq!(addr.to_string().parse::<ViewAddress>().expect("parse"), addr);
    }

    #[test]
    fn repo_scope_key_round_trips_through_optional_parameter_channel() {
        let addr = ViewAddress::Repo {
            identity: RepoIdentity { authority: "github.com".into(), path: "flotilla-org/flotilla".into() },
            repository_key: Some(RepositoryKey("repo/key with space".into())),
        };
        assert_eq!(addr.to_string(), "repo/github.com/flotilla-org/flotilla?key=repo%2Fkey%20with%20space");
        assert_eq!(addr.to_string().parse::<ViewAddress>().expect("parse"), addr);
    }

    #[test]
    fn optional_parameter_channel_rejects_unknown_duplicate_and_malformed_parameters() {
        assert!("repo/github.com/o/r?host=feta".parse::<ViewAddress>().is_err());
        assert!("repo/github.com/o/r?key=a&key=b".parse::<ViewAddress>().is_err());
        assert!("repo/github.com/o/r?key".parse::<ViewAddress>().is_err());
        assert!("project/ns/name?key=repo".parse::<ViewAddress>().is_err());
    }

    #[test]
    fn repo_round_trips_absolute_path_fallback_identities() {
        // A repo with no remote gets `{authority: "local", path: "/abs/path"}`;
        // the leading slash means an empty component, so the path renders as
        // one encoded segment.
        let addr = repo("local", "/tmp/repo-0");
        assert_eq!(addr.to_string(), "repo/local/%2Ftmp%2Frepo-0");
        assert_eq!(addr.to_string().parse::<ViewAddress>().expect("parse"), addr);
    }

    #[test]
    fn node_scoped_kinds_round_trip() {
        for (addr, s) in [
            (ViewAddress::Convoy { namespace: "flotilla".into(), name: "manifest".into() }, "convoy/flotilla/manifest"),
            (
                ViewAddress::Vessel { namespace: "flotilla".into(), convoy: "manifest".into(), vessel: "leg-1".into() },
                "vessel/flotilla/manifest/leg-1",
            ),
            (ViewAddress::Project { namespace: "flotilla".into(), name: "flotilla".into() }, "project/flotilla/flotilla"),
        ] {
            assert_eq!(addr.to_string(), s);
            assert_eq!(s.parse::<ViewAddress>().expect("parse"), addr);
        }
    }

    #[test]
    fn node_scoped_kinds_reject_wrong_arity() {
        assert!("convoy/ns".parse::<ViewAddress>().is_err());
        assert!("convoy/ns/a/b".parse::<ViewAddress>().is_err());
        assert!("vessel/ns/c".parse::<ViewAddress>().is_err());
        assert!("project/ns".parse::<ViewAddress>().is_err());
    }

    #[test]
    fn scheme_prefix_is_accepted() {
        assert_eq!("flotilla://overview".parse::<ViewAddress>().expect("parse"), ViewAddress::Overview);
        assert_eq!("flotilla://repo/github.com/o/r".parse::<ViewAddress>().expect("parse"), repo("github.com", "o/r"));
    }

    #[test]
    fn segments_percent_encode_reserved_characters() {
        let addr = ViewAddress::Convoys { namespace: "with/slash and space".to_string() };
        let rendered = addr.to_string();
        assert_eq!(rendered, "convoys/with%2Fslash%20and%20space");
        assert_eq!(rendered.parse::<ViewAddress>().expect("parse"), addr);
    }

    #[test]
    fn unknown_kind_is_rejected() {
        let err = "portal/xyz".parse::<ViewAddress>().expect_err("must fail");
        assert!(err.contains("unknown view kind"), "unexpected error: {err}");
    }

    #[test]
    fn wrong_arity_is_rejected() {
        assert!("overview/extra".parse::<ViewAddress>().is_err());
        assert!("convoys".parse::<ViewAddress>().is_err());
        assert!("convoys/a/b".parse::<ViewAddress>().is_err());
        assert!("independents/extra".parse::<ViewAddress>().is_err());
        assert!("repo/github.com".parse::<ViewAddress>().is_err());
        assert!("".parse::<ViewAddress>().is_err());
        assert!("convoys//x".parse::<ViewAddress>().is_err());
    }

    #[test]
    fn serde_uses_the_string_form() {
        let addr = repo("github.com", "o/r");
        let json = serde_json::to_string(&addr).expect("serialize");
        assert_eq!(json, "\"repo/github.com/o/r\"");
        assert_eq!(serde_json::from_str::<ViewAddress>(&json).expect("deserialize"), addr);
    }
}
