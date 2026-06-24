// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.
//
// Native reimplementation of /usr/vm/sbin/filewait.
//
// Waits for a path to appear, then prints "<path> now exists" and exits 0.
// Usage error exits 2 (matches the node tool). With no --timeout the process
// blocks forever until the file appears or it is killed, matching node.
//
// The directory may not exist yet (e.g. /.zonecontrol created during boot), so
// we poll for the parent dir, then attach a notify watcher to it and re-check
// the file on every event. notify uses the illumos FEN/event-port backend.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use notify::{RecursiveMode, Watcher};

// Re-check cadence and watcher-attach retry interval; mirrors node's 500ms loop.
const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Parser)]
#[command(
    name = "filewait",
    about = "Wait for a file to appear",
    disable_version_flag = true
)]
struct Args {
    /// Path to wait for.
    filename: PathBuf,

    /// Optional timeout in seconds; without it, wait forever (node behavior).
    #[arg(short = 't', long = "timeout", value_name = "SECONDS")]
    timeout: Option<u64>,
}

fn main() {
    // clap exits 2 on usage error, matching node's process.exit(2).
    let args = Args::parse();

    match run(&args.filename, args.timeout) {
        Ok(true) => {
            println!("{} now exists", args.filename.display());
            std::process::exit(0);
        }
        Ok(false) => {
            // Timed out.
            eprintln!("timed out waiting for {}", args.filename.display());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("filewait: {e:#}");
            std::process::exit(1);
        }
    }
}

// Returns Ok(true) when the file exists, Ok(false) on timeout.
fn run(filename: &Path, timeout: Option<u64>) -> Result<bool> {
    let deadline = timeout.map(|s| Instant::now() + Duration::from_secs(s));

    // Fast path: already there.
    if filename.exists() {
        return Ok(true);
    }

    let dir = parent_dir(filename);

    // Wait for the parent directory before we can watch it.
    loop {
        if dir.exists() {
            break;
        }
        if filename.exists() {
            return Ok(true);
        }
        if past_deadline(deadline) {
            return Ok(false);
        }
        std::thread::sleep(poll_step(deadline));
    }

    let (tx, rx) = channel();
    let mut watcher =
        notify::recommended_watcher(tx).context("failed to create filesystem watcher")?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .with_context(|| format!("failed to watch {}", dir.display()))?;

    // Re-check after attaching: the file may have appeared in the race window,
    // in which case no event will ever be delivered.
    if filename.exists() {
        return Ok(true);
    }

    loop {
        let wait = poll_step(deadline);
        match rx.recv_timeout(wait) {
            // Event (or watcher error) on the dir; we don't care which, just
            // re-check existence like node does.
            Ok(_) => {
                if filename.exists() {
                    return Ok(true);
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // Periodic re-check guards against missed events.
                if filename.exists() {
                    return Ok(true);
                }
                if past_deadline(deadline) {
                    return Ok(false);
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("watcher thread disconnected");
            }
        }
    }
}

fn parent_dir(filename: &Path) -> PathBuf {
    match filename.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        // path::dirname(".") semantics for a bare filename.
        _ => PathBuf::from("."),
    }
}

fn past_deadline(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

// Sleep/recv step capped so we re-poll periodically and honor the deadline.
fn poll_step(deadline: Option<Instant>) -> Duration {
    match deadline {
        Some(d) => POLL_INTERVAL.min(d.saturating_duration_since(Instant::now())),
        None => POLL_INTERVAL,
    }
}
