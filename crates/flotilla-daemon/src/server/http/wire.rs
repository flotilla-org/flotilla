//! Shared HTTP wire rules for commands and tables.
use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::{json, Value};
pub(super) const LIMITS: flotilla_cbor::Limits =
    flotilla_cbor::Limits { max_message: 16 * 1024 * 1024, max_depth: 64, max_elements: 1 << 20, max_byte_string: 16 * 1024 * 1024 };
/// Specific media ranges override wildcards, including explicit exclusions.
pub(super) fn negotiate(accept: &str, allowed: &[&str]) -> Result<usize, &'static str> {
    if accept.trim().is_empty() {
        return Ok(0);
    }
    let mut candidates = vec![None::<(u8, f32)>; allowed.len()];
    for entry in accept.split(',') {
        let mut parts = entry.trim().split(';');
        let media = parts.next().unwrap_or_default().trim().to_ascii_lowercase();
        let quality = parts
            .find_map(|p| p.split_once('=').filter(|(key, _)| key.trim().eq_ignore_ascii_case("q")).map(|(_, value)| value.trim()))
            .unwrap_or("1")
            .parse::<f32>()
            .map_err(|_| "invalid Accept quality")?;
        if !quality.is_finite() || !(0.0..=1.0).contains(&quality) {
            return Err("invalid Accept quality");
        }
        for (index, target) in allowed.iter().enumerate() {
            let specificity = if media == *target {
                2
            } else if media == "*/*" {
                0
            } else if media.ends_with("/*") && target.starts_with(media.trim_end_matches('*')) {
                1
            } else {
                continue;
            };
            if candidates[index].is_none_or(|(old, _)| specificity >= old) {
                candidates[index] = Some((specificity, quality))
            }
        }
    }
    candidates
        .iter()
        .enumerate()
        .filter_map(|(index, value)| value.filter(|(_, q)| *q > 0.0).map(|(_, q)| (index, q)))
        .max_by(|(ia, a), (ib, b)| a.total_cmp(b).then(ib.cmp(ia)))
        .map(|(index, _)| index)
        .ok_or("no acceptable representation")
}
#[derive(Clone, Copy)]
pub(super) enum Encoding {
    Json,
    Cbor,
}

impl Encoding {
    /// A malformed Accept may still identify the error representation, consistently
    /// for command objects and table sequences.
    pub(super) fn error_encoding(headers: &axum::http::HeaderMap) -> Self {
        let accept = headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()).unwrap_or("");
        match negotiate(
            accept,
            &["application/json", "application/cbor", "application/cbor-seq", "application/x-ndjson", "application/jsonl"],
        ) {
            Ok(1 | 2) => Self::Cbor,
            Ok(_) => Self::Json,
            Err("invalid Accept quality")
                if accept.split(',').any(|range| {
                    matches!(
                        range.split(';').next().unwrap_or_default().trim().to_ascii_lowercase().as_str(),
                        "application/cbor" | "application/cbor-seq"
                    )
                }) =>
            {
                Self::Cbor
            }
            Err(_) => Self::Json,
        }
    }

    pub(super) fn response(headers: &axum::http::HeaderMap) -> Result<Self, &'static str> {
        let accept =
            headers.get(header::ACCEPT).map(|value| value.to_str().map_err(|_| "invalid Accept header")).transpose()?.unwrap_or("");
        Ok(match negotiate(accept, &["application/json", "application/cbor"])? {
            0 => Self::Json,
            _ => Self::Cbor,
        })
    }

    pub(super) fn render(self, status: StatusCode, value: impl Serialize) -> Response {
        let mut value = match serde_json::to_value(value) {
            Ok(value) => value,
            Err(error) => return self.error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
        };
        let result = match self {
            Self::Cbor => flotilla_cbor::to_vec(&value, &LIMITS).map(|bytes| ("application/cbor", bytes)).map_err(|e| e.to_string()),
            Self::Json => {
                large_integers(&mut value);
                serde_json::to_vec(&value).map(|bytes| ("application/json", bytes)).map_err(|e| e.to_string())
            }
        };
        match result {
            Ok((content_type, bytes)) => {
                (status, [(header::CONTENT_TYPE, content_type), (header::CACHE_CONTROL, "no-store")], bytes).into_response()
            }
            Err(error) => self.error(StatusCode::INTERNAL_SERVER_ERROR, error),
        }
    }

    pub(super) fn error(self, status: StatusCode, message: impl AsRef<str>) -> Response {
        self.error_code(status, "command_error", message)
    }
    pub(super) fn error_code(self, status: StatusCode, code: &str, message: impl AsRef<str>) -> Response {
        let mut response = self.render(status, json!({"code":code,"message":message.as_ref()}));
        if status == StatusCode::UNAUTHORIZED {
            response.headers_mut().insert(header::WWW_AUTHENTICATE, axum::http::HeaderValue::from_static("Bearer"));
        }
        response
    }
}

pub(super) fn large_integers(value: &mut Value) {
    const SAFE: u64 = (1 << 53) - 1;
    match value {
        Value::Number(n) if n.as_u64().is_some_and(|n| n > SAFE) => *value = json!({"u64": n.to_string()}),
        Value::Number(n) if n.as_i64().is_some_and(|n| n < -(SAFE as i64)) => *value = json!({"i64": n.to_string()}),
        Value::Array(values) => values.iter_mut().for_each(large_integers),
        Value::Object(values) => values.values_mut().for_each(large_integers),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_accept_rules_cover_case_spaces_exclusions_and_invalid_quality() {
        let media = ["application/json", "application/cbor"];
        assert_eq!(negotiate("Application/CBOR; q = 0.5, application/json; q=0", &media), Ok(1));
        assert_eq!(negotiate("application/json;q=0,*/*;q=1", &media), Ok(1));
        assert!(negotiate("application/json;q=no", &media).is_err());
        assert!(negotiate("application/json;q=NaN", &media).is_err());
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(header::ACCEPT, axum::http::HeaderValue::from_static("application/cbor;q=0,application/json;q=1"));
        assert!(matches!(Encoding::error_encoding(&headers), Encoding::Json));
    }
    #[tokio::test]
    async fn table_and_command_integer_profiles_preserve_full_u64() {
        let message = flotilla_protocol::table::Message::Count { count: u64::MAX, estimate: false, sequence: u64::MAX };
        for encoding in [Encoding::Json, Encoding::Cbor] {
            let response = encoding.render(StatusCode::OK, &message);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            let bytes = axum::body::to_bytes(response.into_body(), 1024).await.unwrap();
            let decoded: flotilla_protocol::table::Message = match encoding {
                Encoding::Json => serde_json::from_slice(&bytes).unwrap(),
                Encoding::Cbor => flotilla_cbor::from_slice(&bytes, &LIMITS).unwrap(),
            };
            assert_eq!(decoded, message);
        }
    }
}
