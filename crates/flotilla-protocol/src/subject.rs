use serde::{Deserialize, Serialize};

use crate::{provider_data::IssueSource, LeafAddress};

/// Identity shared by convoy links, leaves and relay observations.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Subject {
    pub kind: SubjectKind,
    pub source: IssueSource,
    pub id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubjectKind {
    ChangeRequest,
    Issue,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    WorksOn,
    Produces,
    Adopts,
    Supersedes,
    References,
}

impl std::str::FromStr for Relationship {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "works_on" => Ok(Self::WorksOn),
            "produces" => Ok(Self::Produces),
            "adopts" => Ok(Self::Adopts),
            "supersedes" => Ok(Self::Supersedes),
            "references" => Ok(Self::References),
            _ => Err(format!("unknown subject relationship `{value}`")),
        }
    }
}

impl Relationship {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WorksOn => "works_on",
            Self::Produces => "produces",
            Self::Adopts => "adopts",
            Self::Supersedes => "supersedes",
            Self::References => "references",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryAlias {
    pub project: Option<String>,
    pub alias: String,
    pub source: IssueSource,
    /// Public forge root, for example `https://github.com`.
    pub web_base: String,
    /// Optional short qualifier for a non-default forge.
    pub forge_alias: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ReferenceContext {
    pub repositories: Vec<RepositoryAlias>,
}

impl Subject {
    pub fn from_leaf(address: &LeafAddress) -> Option<Self> {
        let (kind, service, scope, number) = match address {
            LeafAddress::ChangeRequest { service, scope, number } => (SubjectKind::ChangeRequest, service, scope, number),
            LeafAddress::Issue { service, scope, number } => (SubjectKind::Issue, service, scope, number),
            _ => return None,
        };
        Some(Self { kind, source: IssueSource { service: service.clone(), scope: scope.clone() }, id: number.to_string() })
    }

    pub fn leaf(&self) -> Result<LeafAddress, String> {
        let number = self.id.parse::<u64>().map_err(|_| format!("subject id `{}` is not a numeric forge number", self.id))?;
        Ok(match self.kind {
            SubjectKind::ChangeRequest => {
                LeafAddress::ChangeRequest { service: self.source.service.clone(), scope: self.source.scope.clone(), number }
            }
            SubjectKind::Issue => LeafAddress::Issue { service: self.source.service.clone(), scope: self.source.scope.clone(), number },
        })
    }

    pub fn internal(&self) -> Result<String, String> {
        Ok(self.leaf()?.to_string())
    }

    pub fn url(&self, context: &ReferenceContext) -> Option<String> {
        let base = context
            .repositories
            .iter()
            .find(|repo| repo.source == self.source)
            .map(|repo| repo.web_base.as_str())
            .or_else(|| (self.source.service == "github.com").then_some("https://github.com"))?;
        let path = match self.kind {
            SubjectKind::ChangeRequest => "pull",
            SubjectKind::Issue => "issues",
        };
        Some(format!("{}/{}/{}/{}", base.trim_end_matches('/'), self.source.scope, path, self.id))
    }

    pub fn short(&self, context: &ReferenceContext) -> String {
        let mark = match self.kind {
            SubjectKind::ChangeRequest => '!',
            SubjectKind::Issue => '#',
        };
        let matches = context.repositories.iter().filter(|repo| repo.source == self.source).collect::<Vec<_>>();
        if let Some(repo) = matches.first() {
            let same_alias = context.repositories.iter().filter(|candidate| candidate.alias == repo.alias).count();
            if same_alias == 1 {
                return format!("{}{mark}{}", repo.alias, self.id);
            }
            if let Some(project) = &repo.project {
                let same_project_alias = context
                    .repositories
                    .iter()
                    .filter(|candidate| candidate.project.as_ref() == Some(project) && candidate.alias == repo.alias)
                    .count();
                if same_project_alias == 1 {
                    return format!("{project}/{}{mark}{}", repo.alias, self.id);
                }
            }
            if let Some(forge) = &repo.forge_alias {
                return format!("{forge}:{}{mark}{}", self.source.scope, self.id);
            }
        }
        if self.source.service == "github.com" {
            return format!("{}{mark}{}", self.source.scope, self.id);
        }
        self.internal().unwrap_or_else(|_| format!("{}{mark}{}", self.source.scope, self.id))
    }
}

impl ReferenceContext {
    pub fn parse(&self, value: &str) -> Result<Subject, String> {
        if value.starts_with("cr/") || value.starts_with("issue/") {
            return Subject::from_leaf(&value.parse::<LeafAddress>()?).ok_or_else(|| format!("invalid subject reference `{value}`"));
        }
        if value.starts_with("https://") || value.starts_with("http://") {
            let url = url::Url::parse(value).map_err(|error| format!("invalid subject URL `{value}`: {error}"))?;
            let base = format!("{}://{}", url.scheme(), url.host_str().ok_or_else(|| format!("invalid subject URL `{value}`"))?);
            let root = self.repositories.iter().filter(|entry| value.starts_with(&entry.web_base)).max_by_key(|entry| entry.web_base.len());
            let path = if let Some(root) = root {
                url.path().strip_prefix(root.web_base.strip_prefix(&base).unwrap_or("")).unwrap_or(url.path())
            } else {
                url.path()
            };
            let path = path.trim_matches('/').split('/').collect::<Vec<_>>();
            let [owner, repo, kind, id] = path.as_slice() else { return Err(format!("invalid subject URL `{value}`")) };
            let kind = match *kind {
                "pull" | "pulls" => SubjectKind::ChangeRequest,
                "issues" => SubjectKind::Issue,
                _ => return Err(format!("invalid subject URL `{value}`")),
            };
            let scope = format!("{owner}/{repo}");
            let service = self
                .repositories
                .iter()
                .find(|entry| entry.source.scope == scope && (entry.web_base == base || entry.web_base.starts_with(&format!("{base}/"))))
                .map_or_else(|| url.host_str().unwrap_or_default().to_string(), |entry| entry.source.service.clone());
            return Self::subject(kind, IssueSource { service, scope }, id);
        }
        let (stem, kind, id) = if let Some((stem, id)) = value.rsplit_once('!') {
            (stem, SubjectKind::ChangeRequest, id)
        } else if let Some((stem, id)) = value.rsplit_once('#') {
            (stem, SubjectKind::Issue, id)
        } else {
            return Err(format!("invalid subject reference `{value}`"));
        };
        let candidates = self.repositories.iter().filter(|repo| {
            if let Some((forge, scope)) = stem.split_once(':') {
                repo.forge_alias.as_deref() == Some(forge) && repo.source.scope == scope
            } else if let Some((project, alias)) = stem.split_once('/') {
                (repo.project.as_deref() == Some(project) && repo.alias == alias)
                    || (repo.source.service == "github.com" && repo.source.scope == stem)
            } else {
                repo.alias == stem
            }
        });
        let mut sources = candidates.map(|repo| &repo.source);
        let source = match (sources.next(), sources.next()) {
            (Some(source), None) => source.clone(),
            (Some(_), Some(_)) => return Err(format!("ambiguous subject reference `{value}`")),
            (None, _) if stem.contains('/') && !stem.contains(':') => {
                IssueSource { service: "github.com".to_string(), scope: stem.to_string() }
            }
            _ => return Err(format!("unknown repository in subject reference `{value}`")),
        };
        Self::subject(kind, source, id)
    }

    fn subject(kind: SubjectKind, source: IssueSource, id: &str) -> Result<Subject, String> {
        let number = id.parse::<u64>().map_err(|_| format!("invalid subject number `{id}`"))?;
        Ok(Subject { kind, source, id: number.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip_across_forms_and_forges() {
        let context = ReferenceContext {
            repositories: vec![
                RepositoryAlias {
                    project: Some("wheelhouse".into()),
                    alias: "cleat".into(),
                    source: IssueSource { service: "github.com".into(), scope: "flotilla-org/cleat".into() },
                    web_base: "https://github.com".into(),
                    forge_alias: None,
                },
                RepositoryAlias {
                    project: Some("lab".into()),
                    alias: "project-map".into(),
                    source: IssueSource { service: "lab".into(), scope: "robert/project-map".into() },
                    web_base: "https://forge.example".into(),
                    forge_alias: Some("lab".into()),
                },
            ],
        };
        for reference in ["cleat!281", "flotilla-org/cleat#281", "lab:robert/project-map!12"] {
            let subject = context.parse(reference).expect("short reference");
            assert_eq!(context.parse(&subject.internal().expect("internal")), Ok(subject.clone()));
            assert_eq!(context.parse(&subject.short(&context)), Ok(subject.clone()));
            assert_eq!(context.parse(&subject.url(&context).expect("URL")), Ok(subject));
        }
        let owner_repo = ReferenceContext::default().parse("flotilla-org/cleat#281").expect("owner/repo issue");
        assert_eq!(owner_repo.short(&ReferenceContext::default()), "flotilla-org/cleat#281");
        assert_eq!(
            context.parse("https://forge.example/robert/project-map/pulls/12").expect("Forgejo URL").short(&context),
            "project-map!12"
        );
    }

    #[test]
    fn project_and_forge_qualification_disambiguate_aliases() {
        let context = ReferenceContext {
            repositories: vec![
                RepositoryAlias {
                    project: Some("wheelhouse".into()),
                    alias: "cleat".into(),
                    source: IssueSource { service: "github.com".into(), scope: "flotilla-org/cleat".into() },
                    web_base: "https://github.com".into(),
                    forge_alias: None,
                },
                RepositoryAlias {
                    project: Some("lab".into()),
                    alias: "cleat".into(),
                    source: IssueSource { service: "lab".into(), scope: "robert/cleat".into() },
                    web_base: "https://forge.example/team".into(),
                    forge_alias: Some("lab".into()),
                },
            ],
        };
        assert!(context.parse("cleat!12").expect_err("alias is ambiguous").contains("ambiguous"));
        let github = context.parse("wheelhouse/cleat!12").expect("project-qualified reference");
        assert_eq!(github.short(&context), "wheelhouse/cleat!12");
        let forgejo = context.parse("lab:robert/cleat!12").expect("forge-qualified reference");
        assert_eq!(context.parse(&forgejo.url(&context).expect("Forgejo URL with path prefix")), Ok(forgejo.clone()));
        assert_eq!(context.parse(&forgejo.short(&context)), Ok(forgejo));
    }
}
