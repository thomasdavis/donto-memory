//! ETag + Cache-Control helpers for the static documentation
//! surfaces (homepage, /agent.md, /llms.txt, /openapi.json,
//! /integration-patterns.md, etc.).
//!
//! Each static asset is fingerprinted once at first access via
//! `OnceLock<String>` — sha256(content), first 16 hex chars. The
//! handler returns:
//!
//!   - `ETag: "<sha>"` on every response
//!   - `Cache-Control: public, max-age=300` (5 minutes)
//!   - `304 Not Modified` (empty body) when the request's
//!     `If-None-Match` matches the asset's ETag
//!
//! 5 minutes is short enough that deploys propagate fast, long
//! enough to cover hot reload loops from automated agents.

use std::sync::OnceLock;

use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use sha2::{Digest, Sha256};

/// Fingerprint `bytes` once and cache the result.
fn etag_for(slot: &'static OnceLock<String>, bytes: &[u8]) -> String {
    slot.get_or_init(|| {
        let mut h = Sha256::new();
        h.update(bytes);
        let full = hex::encode(h.finalize());
        format!("\"{}\"", &full[..16])
    })
    .clone()
}

/// Build a cacheable text/* response.
///
/// - `slot`: a unique `static OnceLock<String>` per asset (caches
///   the ETag so we hash once).
/// - `content_type`: the response Content-Type.
/// - `body`: the static body (compile-time include_str!).
/// - `req_headers`: the request headers (for If-None-Match).
pub fn cacheable(
    slot: &'static OnceLock<String>,
    content_type: &'static str,
    body: &'static str,
    req_headers: &HeaderMap,
) -> Response {
    let etag = etag_for(slot, body.as_bytes());

    if let Some(want) = req_headers.get(header::IF_NONE_MATCH) {
        if want.to_str().map(|s| s == etag).unwrap_or(false) {
            // 304 must not carry a body, but should re-state the
            // ETag and Cache-Control so the client knows the
            // resource still exists.
            return (
                StatusCode::NOT_MODIFIED,
                [
                    (header::ETAG, etag.as_str()),
                    (header::CACHE_CONTROL, "public, max-age=300"),
                ],
            )
                .into_response();
        }
    }
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::ETAG, etag.as_str()),
            (header::CACHE_CONTROL, "public, max-age=300"),
        ],
        body,
    )
        .into_response()
}
