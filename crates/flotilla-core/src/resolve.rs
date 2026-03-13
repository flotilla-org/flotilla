use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq)]
pub enum ResolveError {
    NotFound(String),
    Ambiguous { query: String, candidates: Vec<PathBuf> },
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(q) => write!(f, "no repo matching '{q}'"),
            Self::Ambiguous { query, candidates } => {
                write!(f, "'{query}' matches multiple repos:")?;
                for c in candidates {
                    write!(f, "\n  {}", c.display())?;
                }
                Ok(())
            }
        }
    }
}

pub fn resolve_repo<'a>(query: &str, repos: impl Iterator<Item = (&'a Path, Option<&'a str>)>) -> Result<PathBuf, ResolveError> {
    let entries: Vec<_> = repos.collect();

    // 1. Exact path match
    for &(path, _) in &entries {
        if path.as_os_str() == query {
            return Ok(path.to_path_buf());
        }
    }

    // 2. Exact repo name (last path component)
    for &(path, _) in &entries {
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if name == query {
                return Ok(path.to_path_buf());
            }
        }
    }

    // 3. Exact slug match
    for &(path, slug) in &entries {
        if slug == Some(query) {
            return Ok(path.to_path_buf());
        }
    }

    // 4. Unique substring match against name and slug
    let mut matches: Vec<PathBuf> = Vec::new();
    for &(path, slug) in &entries {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(query) || slug.is_some_and(|s| s.contains(query)) {
            matches.push(path.to_path_buf());
        }
    }

    match matches.len() {
        0 => Err(ResolveError::NotFound(query.to_string())),
        1 => Ok(matches.into_iter().next().expect("checked len")),
        _ => Err(ResolveError::Ambiguous { query: query.to_string(), candidates: matches }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repos() -> Vec<(PathBuf, Option<String>)> {
        vec![
            (PathBuf::from("/home/user/dev/flotilla"), Some("rjwittams/flotilla".into())),
            (PathBuf::from("/home/user/dev/other-project"), Some("org/other-project".into())),
            (PathBuf::from("/home/user/dev/flotilla-fork"), Some("someone/flotilla".into())),
        ]
    }

    fn iter(repos: &[(PathBuf, Option<String>)]) -> impl Iterator<Item = (&Path, Option<&str>)> {
        repos.iter().map(|(p, s)| (p.as_path(), s.as_deref()))
    }

    #[test]
    fn exact_path_match() {
        let r = repos();
        let result = resolve_repo("/home/user/dev/flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/flotilla")));
    }

    #[test]
    fn exact_name_match() {
        let r = repos();
        let result = resolve_repo("other-project", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/other-project")));
    }

    #[test]
    fn exact_slug_match() {
        let r = repos();
        let result = resolve_repo("rjwittams/flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/flotilla")));
    }

    #[test]
    fn unique_substring_match() {
        let r = repos();
        let result = resolve_repo("other", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/home/user/dev/other-project")));
    }

    #[test]
    fn ambiguous_substring() {
        let r = repos();
        // "flot" is a substring of both "flotilla" and "flotilla-fork"
        // but matches no exact name or slug, so it's ambiguous
        let result = resolve_repo("flot", iter(&r));
        assert!(matches!(result, Err(ResolveError::Ambiguous { .. })));
        if let Err(ResolveError::Ambiguous { candidates, .. }) = result {
            assert_eq!(candidates.len(), 2);
        }
    }

    #[test]
    fn not_found() {
        let r = repos();
        let result = resolve_repo("nonexistent", iter(&r));
        assert!(matches!(result, Err(ResolveError::NotFound(_))));
    }

    #[test]
    fn exact_match_takes_priority_over_substring() {
        // "flotilla-fork" has "flotilla" as a substring, but exact name
        // "flotilla" should match the first repo, not be ambiguous
        let r = vec![
            (PathBuf::from("/a/flotilla"), Some("rjwittams/flotilla".into())),
            (PathBuf::from("/a/flotilla-fork"), Some("someone/flotilla".into())),
        ];
        let result = resolve_repo("flotilla", iter(&r));
        assert_eq!(result, Ok(PathBuf::from("/a/flotilla")));
    }
}
