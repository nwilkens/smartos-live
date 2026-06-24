// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! IMGAPI v2 HTTP client (reqwest + rustls).
//!
//! Endpoints used by imgadm's consume path:
//! - `GET <url>/images`           — list (avail)
//! - `GET <url>/images/<uuid>`    — single manifest (show/import)
//! - `GET <url>/images/<uuid>/file` — gzipped `zfs send` stream
//!
//! Manifests are kept as opaque `serde_json::Value` so that
//! sub-second `published_at` precision and non-string `tags` values
//! round-trip exactly the way node's `JSON.parse`/`JSON.stringify`
//! preserve them.

use bytes::Bytes;
use futures_util::Stream;
use serde_json::Value;

use crate::config::Source;
use crate::error::{Error, Result};

/// IMGAPI client bound to one source URL.
pub struct ImgapiClient {
    http: reqwest::Client,
    base: String,
    channel: Option<String>,
}

impl ImgapiClient {
    pub fn new(source: &Source, user_agent: &str) -> Result<ImgapiClient> {
        let insecure = match source.insecure {
            Some(v) => v,
            // Mirror node: IMGADM_INSECURE=1 disables verification.
            None => std::env::var("IMGADM_INSECURE").as_deref() == Ok("1"),
        };
        let http = reqwest::Client::builder()
            .user_agent(user_agent.to_string())
            .danger_accept_invalid_certs(insecure)
            .build()?;
        Ok(ImgapiClient {
            http,
            base: source.norm_url(),
            channel: source.channel.clone(),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// `GET /images` — returns the array of manifests.
    pub async fn list_images(&self) -> Result<Vec<Value>> {
        let mut req = self.http.get(self.url("/images"));
        if let Some(ch) = &self.channel {
            req = req.query(&[("channel", ch.as_str())]);
        }
        let resp = req.send().await?.error_for_status()?;
        let body: Value = resp.json().await?;
        match body {
            Value::Array(a) => Ok(a),
            other => Err(Error::Internal(format!(
                "unexpected /images response: {other}"
            ))),
        }
    }

    /// `GET /images/<uuid>` — single manifest. Returns `Ok(None)` on
    /// a 404 (node treats a missing image as "not from this source").
    pub async fn get_image(&self, uuid: &str) -> Result<Option<Value>> {
        let mut req = self.http.get(self.url(&format!("/images/{uuid}")));
        if let Some(ch) = &self.channel {
            req = req.query(&[("channel", ch.as_str())]);
        }
        let resp = req.send().await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        Ok(Some(resp.json().await?))
    }

    /// `GET /images/<uuid>/file` — streaming body plus the
    /// `Content-Length`, if the server provided one.
    pub async fn get_image_file_stream(
        &self,
        uuid: &str,
    ) -> Result<(Option<u64>, impl Stream<Item = reqwest::Result<Bytes>>)> {
        let mut req = self.http.get(self.url(&format!("/images/{uuid}/file")));
        if let Some(ch) = &self.channel {
            req = req.query(&[("channel", ch.as_str())]);
        }
        let resp = req.send().await?.error_for_status()?;
        let len = resp.content_length();
        Ok((len, resp.bytes_stream()))
    }

    /// `GET /ping` — used by `sources -c`. Returns Ok(()) if the
    /// server responds 200 with an imgapi ping body.
    pub async fn ping(&self) -> Result<()> {
        let resp = self.http.get(self.url("/ping")).send().await?;
        if !resp.status().is_success() {
            return Err(Error::Internal(format!(
                "statusCode {}",
                resp.status().as_u16()
            )));
        }
        let body: Value = resp.json().await.map_err(|e| {
            Error::Internal(format!("response not JSON: {e}"))
        })?;
        if body.get("imgapi").is_some() {
            Ok(())
        } else {
            Err(Error::Internal("not an IMGAPI server".to_string()))
        }
    }
}
