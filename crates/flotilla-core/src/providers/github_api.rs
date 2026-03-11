use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::providers::{run_output, ChannelLabel, CommandRunner};

use async_trait::async_trait;

const MAX_PER_PAGE: usize = 100;

/// Clamp a limit to GitHub's max per_page (100), warning if truncated.
pub fn clamp_per_page(limit: usize) -> usize {
    if limit > MAX_PER_PAGE {
        tracing::warn!(requested = %limit, max = MAX_PER_PAGE, "GitHub API per_page capped");
        MAX_PER_PAGE
    } else {
        limit
    }
}

/// Parsed response from `gh api --include`.
pub struct GhApiResponse {
    pub status: u16,
    pub etag: Option<String>,
    pub body: String,
    pub has_next_page: bool,
    pub total_count: Option<u32>,
}

/// Parse the combined headers+body output from `gh api --include`.
pub fn parse_gh_api_response(raw: &str) -> GhApiResponse {
    // Split on first blank line (headers end with \r\n\r\n or \n\n)
    let (header_section, body) = if let Some(pos) = raw.find("\r\n\r\n") {
        (&raw[..pos], raw[pos + 4..].trim().to_string())
    } else if let Some(pos) = raw.find("\n\n") {
        (&raw[..pos], raw[pos + 2..].trim().to_string())
    } else {
        (raw, String::new())
    };

    let mut status = 0u16;
    let mut etag = None;
    let mut has_next_page = false;

    for (i, line) in header_section.lines().enumerate() {
        if i == 0 {
            // "HTTP/2.0 200 OK" or "HTTP/1.1 304 Not Modified"
            if let Some(code_str) = line.split_whitespace().nth(1) {
                status = code_str.parse().unwrap_or(0);
            }
        } else if line.len() >= 6 && line[..5].eq_ignore_ascii_case("etag:") {
            etag = Some(line[5..].trim().to_string());
        } else if line.len() >= 6 && line[..5].eq_ignore_ascii_case("link:") {
            has_next_page = line.contains("rel=\"next\"");
        }
    }

    GhApiResponse {
        status,
        etag,
        body,
        has_next_page,
        total_count: None,
    }
}

#[async_trait]
pub trait GhApi: Send + Sync {
    async fn get(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<String, String>;
    async fn get_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<GhApiResponse, String>;
}

/// Cache entry: ETag + the JSON response body from last 200.
struct CacheEntry {
    etag: String,
    body: String,
    has_next_page: bool,
}

/// Client that wraps `gh api` with ETag-based conditional request caching.
pub struct GhApiClient {
    cache: Mutex<HashMap<String, CacheEntry>>,
    runner: Arc<dyn CommandRunner>,
}

impl GhApiClient {
    pub fn new(runner: Arc<dyn CommandRunner>) -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            runner,
        }
    }
}

#[async_trait]
impl GhApi for GhApiClient {
    /// Fetch a GitHub API endpoint, using cached ETag for conditional requests.
    /// Returns the JSON body (from cache on 304, fresh on 200).
    async fn get(
        &self,
        endpoint: &str,
        repo_root: &Path,
        label: &ChannelLabel,
    ) -> Result<String, String> {
        self.get_with_headers(endpoint, repo_root, label)
            .await
            .map(|r| r.body)
    }

    async fn get_with_headers(
        &self,
        endpoint: &str,
        repo_root: &Path,
        _label: &ChannelLabel,
    ) -> Result<GhApiResponse, String> {
        // Build args
        let cached_etag = {
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            cache.get(endpoint).map(|e| e.etag.clone())
        };

        let mut args = vec![
            "api".to_string(),
            "--include".to_string(),
            endpoint.to_string(),
        ];
        if let Some(ref etag) = cached_etag {
            args.push("-H".to_string());
            args.push(format!("If-None-Match: {}", etag));
        }

        let args_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

        let output = run_output!(self.runner, "gh", &args_refs, repo_root)?;

        // Always parse stdout — gh api --include writes headers even on 304
        let parsed = parse_gh_api_response(&output.stdout);

        if parsed.status == 304 {
            // Serve from cache
            let cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(entry) = cache.get(endpoint) {
                return Ok(GhApiResponse {
                    status: 304,
                    etag: Some(entry.etag.clone()),
                    body: entry.body.clone(),
                    has_next_page: entry.has_next_page,
                    total_count: None,
                });
            }
            return Err("304 but no cached response".to_string());
        }

        if !output.success {
            return Err(output.stderr);
        }

        if let Some(ref etag) = parsed.etag {
            let mut cache = self.cache.lock().unwrap_or_else(|p| p.into_inner());
            cache.insert(
                endpoint.to_string(),
                CacheEntry {
                    etag: etag.clone(),
                    body: parsed.body.clone(),
                    has_next_page: parsed.has_next_page,
                },
            );
        }

        Ok(parsed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_200_response_extracts_etag_and_body() {
        let raw = "HTTP/2.0 200 OK\r\nEtag: W/\"abc123\"\r\nContent-Type: application/json\r\n\r\n[{\"number\":1}]";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, Some("W/\"abc123\"".to_string()));
        assert_eq!(result.body, "[{\"number\":1}]");
        assert_eq!(result.status, 200);
    }

    #[test]
    fn parse_304_response_has_no_body() {
        let raw = "HTTP/2.0 304 Not Modified\r\nEtag: \"abc123\"\r\n\r\n";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, Some("\"abc123\"".to_string()));
        assert_eq!(result.body, "");
        assert_eq!(result.status, 304);
    }

    #[test]
    fn parse_etag_case_insensitive() {
        // GitHub sends "Etag:" but HTTP spec allows any casing
        for header in ["Etag: \"x\"", "etag: \"x\"", "ETag: \"x\"", "ETAG: \"x\""] {
            let raw = format!("HTTP/2.0 200 OK\r\n{}\r\n\r\n{{}}", header);
            let result = parse_gh_api_response(&raw);
            assert_eq!(
                result.etag,
                Some("\"x\"".to_string()),
                "failed for: {}",
                header
            );
        }
    }

    #[test]
    fn parse_response_without_etag() {
        let raw = "HTTP/2.0 200 OK\r\nContent-Type: text/plain\r\n\r\nhello";
        let result = parse_gh_api_response(raw);
        assert_eq!(result.etag, None);
        assert_eq!(result.body, "hello");
    }

    #[test]
    fn parse_link_header_has_next() {
        let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=2>; rel=\"next\", <https://api.github.com/repos/foo/bar/issues?page=5>; rel=\"last\"\r\nEtag: \"abc\"\r\n\r\n[{\"number\":1}]";
        let result = parse_gh_api_response(raw);
        assert!(result.has_next_page);
        assert_eq!(result.total_count, None);
    }

    #[test]
    fn parse_link_header_no_next() {
        let raw = "HTTP/2.0 200 OK\r\nLink: <https://api.github.com/repos/foo/bar/issues?page=3>; rel=\"prev\"\r\nEtag: \"abc\"\r\n\r\n[]";
        let result = parse_gh_api_response(raw);
        assert!(!result.has_next_page);
    }

    #[test]
    fn parse_no_link_header() {
        let raw = "HTTP/2.0 200 OK\r\nEtag: \"abc\"\r\n\r\n[]";
        let result = parse_gh_api_response(raw);
        assert!(!result.has_next_page);
    }
}
