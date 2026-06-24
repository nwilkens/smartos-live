// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! imgadm error type. Variants mirror the node `lib/errors.js`
//! messages closely enough that operator-facing output matches.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),

    #[error("imgadm has no configured sources")]
    NoSources,

    #[error("image {uuid} is not installed")]
    ImageNotInstalled { uuid: String },

    #[error("an image with uuid \"{0}\" was not found")]
    ImageNotFound(String),

    #[error("an active image \"{0}\" was not found")]
    ActiveImageNotFound(String),

    #[error("image {uuid} has dependent clones: {clones}")]
    ImageHasDependentClones { uuid: String, clones: String },

    #[error("error downloading image: {0}")]
    Download(String),

    #[error("invalid manifest: {0}")]
    InvalidManifest(String),

    #[error("uncompression error: {0}")]
    Uncompression(String),

    #[error(
        "unexpected number of snapshots for image {uuid} ({count}): {snaps}"
    )]
    UnexpectedSnapshots {
        uuid: String,
        count: usize,
        snaps: String,
    },

    #[error("{0}")]
    Internal(String),

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
