// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The custom vmadmd control plane (HTTP/1.1 on 127.0.0.1:8080).
//!
//! This is NOT a REST/vminfod API: `GET /vms`, `/ping`, `/status` all 404. The
//! only client is VM.js. Routes reproduced from node `vmadmd.js`:
//!
//! * `GET  /vm/:uuid/info[?types=t1,t2,...]` -> 200 pretty JSON | 500
//! * `POST /vm/:uuid?action=stop&timeout=<s>` -> 202 | 400 | 500
//! * `POST /vm/:uuid?action=reset` -> 202 | 500
//! * `POST /vm/:uuid?action=sysrq&request=<nmi|screenshot>` -> 202 | 400 | 500
//! * `POST /vm/:uuid?action=reload_display` -> 202 | 404
//! * everything else -> 404 (empty body)
//!
//! POST args come from the URL query merged with an
//! `application/x-www-form-urlencoded` body (node `qs.parse(body)` over the
//! query args). node's client (postVmadmd) actually sends args in the query
//! string and ignores the response status, so the query path is the one that
//! matters in practice; the body merge is preserved for parity.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::state::{Daemon, VerbResult};

const JSON: &str = "application/json";

/// Build the axum router. node strips empty path segments and requires
/// `c = ['vm', uuid, 'info']` for GET and `c = ['vm', uuid]` for POST; axum's
/// typed routes give the same shape, and a catch-all fallback covers the rest.
pub fn router(daemon: Arc<Daemon>) -> Router {
    Router::new()
        .route("/vm/{uuid}/info", get(get_info))
        .route("/vm/{uuid}", post(post_action))
        .fallback(fallback)
        .with_state(daemon)
}

/// A response with `Content-Type: application/json` and a raw string body
/// (node always sets the JSON content-type, even for plain-text bodies).
fn json_body(status: StatusCode, body: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(JSON))],
        body.into(),
    )
        .into_response()
}

/// An empty-body response with no special content type (node's bare
/// `writeHead(404); end()`).
fn empty(status: StatusCode) -> Response {
    status.into_response()
}

/// Parse `a=b&c=d` form/query data into a map. Values are URL-decoded
/// (percent + `+` for space), matching node's querystring.parse.
fn parse_form(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in s.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        out.insert(url_decode(k), url_decode(v));
    }
    out
}

/// Minimal `application/x-www-form-urlencoded` decoder: `+` -> space and
/// `%XX` -> byte.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `GET /vm/:uuid/info`. node getInfo: `types` defaults to `['all']` when
/// absent; success -> 200 pretty JSON; any infoVM error -> 500 (empty body).
async fn get_info(
    State(daemon): State<Arc<Daemon>>,
    Path(uuid): Path<String>,
    RawQuery(q): RawQuery,
) -> Response {
    let args = parse_form(q.as_deref().unwrap_or(""));
    let types: Vec<String> = match args.get("types") {
        Some(t) if !t.is_empty() => t.split(',').map(str::to_string).collect(),
        _ => vec!["all".to_string()],
    };

    match daemon.info_vm(&uuid, &types).await {
        Ok(body) => json_body(StatusCode::OK, body),
        Err(e) => {
            // node logs the error and writes a 500 with an EMPTY body.
            tracing::error!(uuid, error = %e, "info error");
            json_body(StatusCode::INTERNAL_SERVER_ERROR, String::new())
        }
    }
}

/// `POST /vm/:uuid`. node handlePost: validate action + required args (400 on
/// bad request), then dispatch.
async fn post_action(
    State(daemon): State<Arc<Daemon>>,
    Path(uuid): Path<String>,
    RawQuery(q): RawQuery,
    body: Bytes,
) -> Response {
    // node merges the urlencoded POST body over the URL query args.
    let mut args = parse_form(q.as_deref().unwrap_or(""));
    let body_str = String::from_utf8_lossy(&body);
    for (k, v) in parse_form(&body_str) {
        args.insert(k, v);
    }

    let action = args.get("action").map(String::as_str).unwrap_or("");
    let valid_action = matches!(action, "stop" | "sysrq" | "reset" | "reload_display");
    let request = args.get("request").map(String::as_str).unwrap_or("");
    let sysrq_ok = action != "sysrq" || matches!(request, "nmi" | "screenshot");
    let stop_ok = action != "stop" || args.contains_key("timeout");

    if !valid_action || !sysrq_ok || !stop_ok {
        // node: 400 with JSON content-type and empty body.
        return json_body(StatusCode::BAD_REQUEST, String::new());
    }

    let result = match action {
        "stop" => {
            let timeout = args
                .get("timeout")
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            daemon.stop_vm(&uuid, timeout).await
        }
        "reset" => daemon.reset_vm(&uuid).await,
        "sysrq" => daemon.sysrq_vm(&uuid, request).await,
        "reload_display" => daemon.reload_display(&uuid).await,
        // unreachable given validation; node returns 500 'Unknown Command'.
        _ => VerbResult::Error("Unknown Command".to_string()),
    };

    match result {
        VerbResult::Accepted(body) => json_body(StatusCode::ACCEPTED, body),
        VerbResult::Error(body) => json_body(StatusCode::INTERNAL_SERVER_ERROR, body),
        // node reload_display load-failure uses a bare 404 with a text body
        // and no JSON content-type.
        VerbResult::NotFound(body) => (StatusCode::NOT_FOUND, body).into_response(),
    }
}

/// Any other path/method -> 404 empty body. This is why `/vms`, `/ping`,
/// `/status` all 404 (node handleGet/handlePost else-branch).
async fn fallback() -> Response {
    empty(StatusCode::NOT_FOUND)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proxy::Registry;
    use crate::vminfo_client::VminfoClient;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    fn app() -> Router {
        // vminfod is unreachable in tests; routes that don't load a VM
        // (validation, fallback) still exercise the contract.
        let daemon = Arc::new(Daemon::new(
            Registry::new(),
            Some("127.0.0.1".parse().unwrap()),
            VminfoClient::new("127.0.0.1".into(), 1, "test".into()),
        ));
        router(daemon)
    }

    async fn status_of(app: Router, method: &str, uri: &str) -> StatusCode {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("req");
        app.oneshot(req).await.expect("resp").status()
    }

    #[tokio::test]
    async fn unknown_get_paths_404() {
        assert_eq!(status_of(app(), "GET", "/vms").await, StatusCode::NOT_FOUND);
        assert_eq!(status_of(app(), "GET", "/ping").await, StatusCode::NOT_FOUND);
        assert_eq!(status_of(app(), "GET", "/status").await, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn post_missing_action_is_400() {
        let s = status_of(app(), "POST", "/vm/abc").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_stop_without_timeout_is_400() {
        let s = status_of(app(), "POST", "/vm/abc?action=stop").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_sysrq_bad_request_is_400() {
        let s = status_of(app(), "POST", "/vm/abc?action=sysrq&request=bogus").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn post_unknown_action_is_400() {
        let s = status_of(app(), "POST", "/vm/abc?action=frobnicate").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn form_parse_decodes() {
        let m = parse_form("action=reload_display&timeout=30&x=a%20b");
        assert_eq!(m.get("action").map(String::as_str), Some("reload_display"));
        assert_eq!(m.get("timeout").map(String::as_str), Some("30"));
        assert_eq!(m.get("x").map(String::as_str), Some("a b"));
    }
}
