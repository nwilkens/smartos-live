// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Bunyan log levels, ANSI color helpers, and name<->number mapping.

pub const TRACE: i64 = 10;
pub const DEBUG: i64 = 20;
pub const INFO: i64 = 30;
pub const WARN: i64 = 40;
pub const ERROR: i64 = 50;
pub const FATAL: i64 = 60;

#[derive(Clone, Copy)]
pub enum ColorMode {
    Auto,
    On,
    Off,
}

pub fn level_from_name(name: &str) -> Option<i64> {
    match name {
        "trace" => Some(TRACE),
        "debug" => Some(DEBUG),
        "info" => Some(INFO),
        "warn" => Some(WARN),
        "error" => Some(ERROR),
        "fatal" => Some(FATAL),
        _ => None,
    }
}

/// `upperPaddedNameFromLevel`: 4-char names get a leading space; unknown
/// levels become `LVL<n>`.
pub fn upper_padded(level: i64) -> String {
    match level {
        TRACE => "TRACE".to_string(),
        DEBUG => "DEBUG".to_string(),
        INFO => " INFO".to_string(),
        WARN => " WARN".to_string(),
        ERROR => "ERROR".to_string(),
        FATAL => "FATAL".to_string(),
        n => format!("LVL{n}"),
    }
}

pub fn color_from_level(level: i64) -> Option<&'static str> {
    match level {
        TRACE => Some("white"),
        DEBUG => Some("yellow"),
        INFO => Some("cyan"),
        WARN => Some("magenta"),
        ERROR => Some("red"),
        FATAL => Some("inverse"),
        _ => None,
    }
}

/// ANSI codes mirroring node-bunyan's `colors` table; `XXX`/unknown is a no-op.
fn codes(color: &str) -> Option<(u8, u8)> {
    Some(match color {
        "bold" => (1, 22),
        "italic" => (3, 23),
        "underline" => (4, 24),
        "inverse" => (7, 27),
        "white" => (37, 39),
        "grey" => (90, 39),
        "black" => (30, 39),
        "blue" => (34, 39),
        "cyan" => (36, 39),
        "green" => (32, 39),
        "magenta" => (35, 39),
        "red" => (31, 39),
        "yellow" => (33, 39),
        _ => return None,
    })
}

pub fn stylize_color(s: &str, color: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    match codes(color) {
        Some((on, off)) => format!("\u{1b}[{on}m{s}\u{1b}[{off}m"),
        None => s.to_string(),
    }
}
