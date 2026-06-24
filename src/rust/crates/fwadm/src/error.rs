// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Error type for the fwrule/fwadm port. Mirrors the node behavior where a
//! single error throws directly and multiple errors aggregate into a
//! `verror.MultiError` whose `.message` joins child messages.

use std::fmt;

/// A firewall rule / fwadm error. `field` echoes node's
/// `InvalidParamError(field, ...)` for parity in JSON error output, where
/// applicable.
#[derive(Debug, Clone)]
pub struct FwError {
    /// The aggregated, human-readable message.
    pub message: String,
    /// The parameter field associated with the error (e.g. "rule", "uuid").
    pub field: Option<String>,
    /// Child errors when this is a MultiError.
    pub children: Vec<FwError>,
    /// Optional error code (e.g. "ENOENT").
    pub code: Option<String>,
}

impl FwError {
    /// A generic error with no field.
    pub fn generic(msg: impl Into<String>) -> Self {
        FwError {
            message: msg.into(),
            field: None,
            children: Vec::new(),
            code: None,
        }
    }

    /// An InvalidParamError for the given field.
    pub fn param(field: impl Into<String>, msg: impl Into<String>) -> Self {
        FwError {
            message: msg.into(),
            field: Some(field.into()),
            children: Vec::new(),
            code: None,
        }
    }

    /// Shorthand for `InvalidParamError('rule', ...)`.
    pub fn rule(msg: impl Into<String>) -> Self {
        FwError::param("rule", msg)
    }

    /// Attach an error code (node `err.code`).
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    /// Build a MultiError from a list of errors. If there is exactly one,
    /// returns it directly (matching node's `errs.length === 1` shortcut).
    pub fn multi(mut errs: Vec<FwError>) -> FwError {
        if errs.len() == 1 {
            return errs.remove(0);
        }
        // verror.MultiError message: "first of N errors: <first.message>"
        let n = errs.len();
        let first = errs.first().map(|e| e.message.clone()).unwrap_or_default();
        FwError {
            message: format!("first of {n} errors: {first}"),
            field: None,
            children: errs,
            code: None,
        }
    }

    /// Flatten into the list of leaf error messages, matching how the CLI
    /// expands a MultiError (`ase_errors`) into one line per child.
    pub fn leaf_messages(&self) -> Vec<String> {
        if self.children.is_empty() {
            vec![self.message.clone()]
        } else {
            self.children.iter().map(|c| c.message.clone()).collect()
        }
    }
}

impl fmt::Display for FwError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for FwError {}

impl From<std::io::Error> for FwError {
    fn from(e: std::io::Error) -> Self {
        let code = match e.kind() {
            std::io::ErrorKind::NotFound => Some("ENOENT".to_string()),
            _ => None,
        };
        FwError {
            message: e.to_string(),
            field: None,
            children: Vec::new(),
            code,
        }
    }
}

impl From<serde_json::Error> for FwError {
    fn from(e: serde_json::Error) -> Self {
        FwError::generic(e.to_string())
    }
}
