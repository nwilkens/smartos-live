// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! A byte-faithful port of node `hrtime.js` (`prettyHrtime`,
//! `hrtimeToString`, `hrtimeDeltaPretty`).
//!
//! vminfod reports monotonic durations as `"<secs>.<9-nanos>s (<rel>)"`
//! strings in `/status` (`uptime`, `lastRefresh`, `createdAgo`, `took`).
//! The format is non-obvious and load-bearing for byte-for-byte parity, so
//! the algorithm here mirrors the node source line for line. Durations are
//! taken from a monotonic [`std::time::Instant`] captured at start, exactly
//! as node uses `process.hrtime()`.

use std::time::{Duration, Instant};

/// A monotonic hrtime, stored as `(seconds, nanoseconds)` with
/// `0 <= nanoseconds < 1e9`, matching node's `[secs, nsecs]` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HrTime {
    pub secs: u64,
    pub nanos: u32,
}

impl HrTime {
    /// The hrtime delta from `earlier` to `now` (`now - earlier`), as node's
    /// `hrtimeDelta`. Saturates at zero rather than panicking on a
    /// non-monotonic pair (a clock that went backwards).
    #[must_use]
    pub fn delta(earlier: Instant, now: Instant) -> Self {
        let d = now.saturating_duration_since(earlier);
        Self {
            secs: d.as_secs(),
            nanos: d.subsec_nanos(),
        }
    }

    /// `hrtimeToString`: `"<secs>.<nanos padded to 9 digits>"`.
    #[must_use]
    pub fn to_node_string(self) -> String {
        format!("{}.{:09}", self.secs, self.nanos)
    }

    /// `prettyHrtime`: `"<secs>.<9-nanos>s (<relative>)"`.
    ///
    /// `relative` is the largest unit (d/h/m/s/ms/us/ns) whose floored value
    /// is `> 0`, formatted as `parseFloat(value.toFixed(2)) + unit`.
    #[must_use]
    pub fn to_pretty(self) -> String {
        format!("{}s ({})", self.to_node_string(), self.relative())
    }

    fn relative(self) -> String {
        // node `times` array, in scan order; `seconds` carries fractional ms.
        let secs = self.secs as f64;
        let nanos = self.nanos as f64;
        let seconds_with_ms = secs + (nanos / 1e6) / 1e3;
        let times: [(f64, &str); 7] = [
            (secs / 60.0 / 60.0 / 24.0, "d"),
            (secs / 60.0 / 60.0, "h"),
            (secs / 60.0, "m"),
            (seconds_with_ms, "s"),
            (nanos / 1e3 / 1e3, "ms"),
            (nanos / 1e3, "us"),
            (nanos, "ns"),
        ];
        for (value, unit) in times {
            if value.floor() > 0.0 {
                return format!("{}{}", trim_fixed2(value), unit);
            }
        }
        "0ns".to_string()
    }
}

/// Reproduce JS `parseFloat(value.toFixed(2))`: round to 2 decimals, then
/// drop trailing zeros (5.00 -> "5", 5.46 -> "5.46", 39.83 -> "39.83").
fn trim_fixed2(value: f64) -> String {
    // toFixed(2) rounds half-away-from-zero; format!("{:.2}") rounds
    // half-to-even. For the magnitudes here (durations) the difference is
    // immaterial, but match JS by rounding explicitly.
    let rounded = (value * 100.0).round() / 100.0;
    let mut s = format!("{rounded:.2}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// A monotonic clock anchor. Captured once at daemon start; every `/status`
/// duration is computed relative to one of these `Instant`s, matching node's
/// `process.hrtime()` deltas.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    /// The process-start anchor, used to render `curTime` (node's
    /// `hrtimeToString(process.hrtime())`, i.e. uptime since the monotonic
    /// epoch). node's `process.hrtime()` is time since an arbitrary point;
    /// we anchor it to start so `curTime` grows like uptime.
    pub start: Instant,
}

impl Clock {
    #[must_use]
    pub fn now() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    /// `hrtimeToString(process.hrtime())` for the current instant.
    #[must_use]
    pub fn cur_time(&self) -> String {
        HrTime::delta(self.start, Instant::now()).to_node_string()
    }

    /// `hrtimeDeltaPretty(now, then)` for an `Instant` captured earlier.
    #[must_use]
    pub fn pretty_since(then: Instant) -> String {
        HrTime::delta(then, Instant::now()).to_pretty()
    }

    /// `hrtimeToString` of an `Instant` relative to the start anchor.
    #[must_use]
    pub fn stamp(&self, at: Instant) -> String {
        HrTime::delta(self.start, at).to_node_string()
    }
}

/// Convert a [`Duration`] directly into an [`HrTime`] (for tests / synthetic
/// deltas).
#[must_use]
pub fn from_duration(d: Duration) -> HrTime {
    HrTime {
        secs: d.as_secs(),
        nanos: d.subsec_nanos(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pretty(secs: u64, nanos: u32) -> String {
        HrTime { secs, nanos }.to_pretty()
    }

    #[test]
    fn to_node_string_pads_nanos() {
        assert_eq!(HrTime { secs: 2390, nanos: 902031479 }.to_node_string(), "2390.902031479");
        assert_eq!(HrTime { secs: 300, nanos: 0 }.to_node_string(), "300.000000000");
    }

    #[test]
    fn pretty_matches_node_examples() {
        // 2390.902031479s -> 39.83m (secs/60 = 39.84..., floor of hours = 0)
        assert_eq!(pretty(2390, 902031479), "2390.902031479s (39.83m)");
        // 20.279018326s -> 20.28s
        assert_eq!(pretty(20, 279018326), "20.279018326s (20.28s)");
        // 290.270451321s -> 4.83m
        assert_eq!(pretty(290, 270451321), "290.270451321s (4.83m)");
        // 314.298361447s -> 5.23m
        assert_eq!(pretty(314, 298361447), "314.298361447s (5.23m)");
    }

    #[test]
    fn pretty_unit_boundaries() {
        // exactly 300s -> "5m"
        assert_eq!(pretty(300, 0), "300.000000000s (5m)");
        // 90s -> 1.5m
        assert_eq!(pretty(90, 0), "90.000000000s (1.5m)");
        // sub-second 523ms
        assert_eq!(pretty(0, 523_000_000), "0.523000000s (523ms)");
        // >= 3600 switches to hours
        assert_eq!(pretty(3600, 0), "3600.000000000s (1h)");
        // >= 86400 switches to days
        assert_eq!(pretty(86400, 0), "86400.000000000s (1d)");
        // zero -> 0ns
        assert_eq!(pretty(0, 0), "0.000000000s (0ns)");
        // microseconds
        assert_eq!(pretty(0, 5_000), "0.000005000s (5us)");
        // nanoseconds
        assert_eq!(pretty(0, 5), "0.000000005s (5ns)");
    }

    #[test]
    fn trim_fixed2_drops_trailing_zeros() {
        assert_eq!(trim_fixed2(5.0), "5");
        assert_eq!(trim_fixed2(5.456), "5.46");
        assert_eq!(trim_fixed2(5.10), "5.1");
        assert_eq!(trim_fixed2(39.83), "39.83");
    }
}
