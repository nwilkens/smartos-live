// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The HTTP interface: GET `/ping` `/status` `/vms` `/vms/:uuid` `/events`,
//! a 404 (plain body, JSON content-type) for any other path, and a 400 for
//! any non-GET method.
//!
//! Responses that carry VM JSON are served as pre-serialized strings (the
//! cache's cached form) so the byte order matches vminfod exactly; axum's
//! `Json` serializer is deliberately bypassed for those. `/events` is a
//! long-lived chunked NDJSON stream with the response timeout disabled.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get};
use futures::StreamExt;
use serde::Deserialize;
use uuid::Uuid;

use crate::events::ready_record;
use crate::state::{Daemon, Listener};

const JSON: &str = "application/json";

/// Build the axum router with the daemon as shared state.
pub fn router(daemon: Arc<Daemon>) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .route("/status", get(status))
        .route("/vms", get(vms))
        .route("/vms/{uuid}", get(vm_by_uuid))
        .route("/events", get(events))
        // Any other path -> 404 with the node body; any non-GET on an
        // unknown path -> 400.
        .fallback(any(fallback))
        // A non-GET on a *known* path (e.g. POST /vms) -> 400 empty body,
        // matching node's `if (req.method !== 'GET')` blanket reject.
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(daemon)
}

/// A response with `Content-Type: application/json` and a raw string body
/// (used for pre-serialized JSON and for the node's plain-text error bodies
/// that nonetheless carry a JSON content-type header).
fn json_body(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, HeaderValue::from_static(JSON))],
        body,
    )
        .into_response()
}

async fn ping() -> Response {
    json_body(StatusCode::OK, "{\"ping\":\"pong\"}".to_string())
}

#[derive(Deserialize, Default)]
struct StatusQuery {
    #[serde(default)]
    full: Option<String>,
}

async fn status(State(daemon): State<Arc<Daemon>>, Query(q): Query<StatusQuery>) -> Response {
    // node: `if (args.full)` is truthy for any present value (?full=true).
    let full = q.full.is_some();
    let v = daemon.status_json(full);
    json_body(
        StatusCode::OK,
        serde_json::to_string(&v).unwrap_or_else(|_| "{}".to_string()),
    )
}

async fn vms(State(daemon): State<Arc<Daemon>>) -> Response {
    let body = daemon.with_inner(|i| i.cache.serialize_vms());
    json_body(StatusCode::OK, body)
}

async fn vm_by_uuid(State(daemon): State<Arc<Daemon>>, Path(uuid): Path<String>) -> Response {
    let cached = daemon.with_inner(|i| i.cache.get(&uuid).map(|c| c.json.clone()));
    match cached {
        Some(json) => json_body(StatusCode::OK, json),
        // node: 404, plain-text body "VM Not Found: <uuid>", JSON header.
        None => json_body(StatusCode::NOT_FOUND, format!("VM Not Found: {uuid}")),
    }
}

/// `GET /events`: register a listener, emit a `ready` frame, then stream
/// every fan-out line as NDJSON. The connection is held open until the client
/// disconnects (axum drops the body stream on disconnect).
async fn events(State(daemon): State<Arc<Daemon>>, headers: HeaderMap) -> Response {
    let conn_uuid = Uuid::new_v4().to_string();
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let created = std::time::Instant::now();
    let created_date = crate::events::now_iso8601();

    daemon.register_listener(
        conn_uuid.clone(),
        Listener {
            user_agent,
            created,
            created_date: created_date.clone(),
        },
    );

    // The ready frame: vms is the doubly-encoded /vms string.
    let vms_json = daemon.with_inner(|i| i.cache.serialize_vms());
    let ready = ready_record(&conn_uuid, &created_date, &vms_json);
    let ready_line = serde_json::to_string(&ready).unwrap_or_else(|_| "{}".to_string());

    let rx = daemon.hub.subscribe();

    // Cleanup guard: removes the listener when the stream is dropped (client
    // disconnect), matching node's `res.on('close', cleanup)`.
    let guard = ListenerGuard {
        daemon: daemon.clone(),
        uuid: conn_uuid,
    };

    let stream = build_event_stream(ready_line, rx, guard);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, JSON)
        // Disable any intermediary buffering for the long-lived stream.
        .header("X-Accel-Buffering", "no")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| json_body(StatusCode::INTERNAL_SERVER_ERROR, String::new()))
}

/// Removes the `/events` listener from `/status` bookkeeping on drop.
struct ListenerGuard {
    daemon: Arc<Daemon>,
    uuid: String,
}

impl Drop for ListenerGuard {
    fn drop(&mut self) {
        self.daemon.remove_listener(&self.uuid);
    }
}

/// Build the NDJSON body stream: the `ready` line first, then every
/// broadcast line, each terminated by `\n`. The `guard` is moved into the
/// stream so it lives exactly as long as the connection.
fn build_event_stream(
    ready_line: String,
    rx: tokio::sync::broadcast::Receiver<String>,
    guard: ListenerGuard,
) -> impl futures::Stream<Item = Result<String, Infallible>> {
    let ready = futures::stream::once(async move { Ok(format!("{ready_line}\n")) });

    let body = futures::stream::unfold(
        (rx, guard),
        |(mut rx, guard)| async move {
            loop {
                match rx.recv().await {
                    Ok(line) => return Some((Ok(format!("{line}\n")), (rx, guard))),
                    // Lagged: skip dropped lines and keep streaming.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    // Sender gone: end the stream (drops the guard).
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    );

    ready.chain(body)
}

/// Fallback for unmatched routes: 400 for non-GET, else 404. node returns 400
/// with an empty body for any non-GET, and 404 with "Unsupported URL Path"
/// (JSON content-type) for an unknown GET path.
async fn fallback(method: Method, _uri: Uri) -> Response {
    if method != Method::GET {
        return json_body(StatusCode::BAD_REQUEST, String::new());
    }
    json_body(StatusCode::NOT_FOUND, "Unsupported URL Path".to_string())
}

/// A non-GET request to a known path. node rejects any non-GET method with a
/// 400 and an empty body (Content-Type still application/json).
async fn method_not_allowed() -> Response {
    json_body(StatusCode::BAD_REQUEST, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::json;
    use tower::ServiceExt;

    fn daemon_with_one_vm() -> Arc<Daemon> {
        let d = Arc::new(Daemon::new());
        d.with_inner(|i| {
            i.cache.insert(
                "4256eeee-c8b0-4fed-9e0a-975a31e14571".to_string(),
                crate::cache::CachedVm::new(json!({
                    "zonename": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
                    "uuid": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
                    "brand": "joyent-minimal"
                })),
            );
        });
        d
    }

    async fn body_string(resp: Response) -> String {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("body");
        String::from_utf8(bytes.to_vec()).expect("utf8")
    }

    #[tokio::test]
    async fn ping_exact() {
        let app = router(Arc::new(Daemon::new()));
        let resp = app
            .oneshot(Request::builder().uri("/ping").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).expect("ct"),
            "application/json"
        );
        assert_eq!(body_string(resp).await, "{\"ping\":\"pong\"}");
    }

    #[tokio::test]
    async fn vms_array_and_single() {
        let app = router(daemon_with_one_vm());
        let resp = app
            .clone()
            .oneshot(Request::builder().uri("/vms").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        let body = body_string(resp).await;
        assert!(body.starts_with('['));
        assert!(body.contains("\"zonename\":\"4256eeee-c8b0-4fed-9e0a-975a31e14571\""));

        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/vms/4256eeee-c8b0-4fed-9e0a-975a31e14571")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_string(resp).await;
        assert!(body.starts_with('{'));
        assert!(body.contains("joyent-minimal"));
    }

    #[tokio::test]
    async fn vm_not_found_is_404_plain_body() {
        let app = router(Arc::new(Daemon::new()));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/vms/00000000-0000-0000-0000-000000000000")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resp.headers().get(header::CONTENT_TYPE).expect("ct"),
            "application/json"
        );
        assert_eq!(
            body_string(resp).await,
            "VM Not Found: 00000000-0000-0000-0000-000000000000"
        );
    }

    #[tokio::test]
    async fn unknown_path_404() {
        let app = router(Arc::new(Daemon::new()));
        let resp = app
            .oneshot(Request::builder().uri("/data").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(body_string(resp).await, "Unsupported URL Path");
    }

    #[tokio::test]
    async fn non_get_is_400_empty() {
        let app = router(Arc::new(Daemon::new()));
        let resp = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/vms")
                    .body(Body::empty())
                    .expect("req"),
            )
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_string(resp).await, "");
    }

    #[tokio::test]
    async fn status_is_json_object() {
        let app = router(Arc::new(Daemon::new()));
        let resp = app
            .oneshot(Request::builder().uri("/status").body(Body::empty()).expect("req"))
            .await
            .expect("resp");
        assert_eq!(resp.status(), StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body_string(resp).await).expect("json");
        assert_eq!(v["numVms"], json!(0));
        assert!(v["uptime"].is_string());
    }
}
