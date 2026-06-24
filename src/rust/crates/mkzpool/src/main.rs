// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! mkzpool: read a disklayout JSON and run `zpool create` for it.
//!
//! Drop-in for src/mkzpool.js. Option handling matches node getopt
//! `BasicParser('Bef', ...)`: short clustered flags, parsing stops at the
//! first non-option operand. Operands are `<pool> <file.json>`.
//!
//! Setting MKZPOOL_DRY_RUN=1 prints the exact program+argv that would be
//! executed and exits without creating anything. This mirrors the live
//! `zpool create` invocation for parity checks; it is not a node feature, so
//! it is gated behind an env var and invisible to normal callers.

mod layout;

use std::fs;
use std::process::{self, Command};

/// Matches node usage(): prints and exits non-zero.
fn usage() -> ! {
    // process.argv[0] in node is the interpreter path; the original prints
    // that verbatim. We use the program name for an equivalent message.
    eprintln!("usage: mkzpool [-Bef] <pool> <file.json>");
    process::exit(255);
}

fn fatal(msg: &str) -> ! {
    println!("fatal error: {msg}");
    process::exit(255);
}

struct Opts {
    efi: bool,
    encrypt: bool,
    force: bool,
}

/// Parse getopt-style `Bef` flags. Returns parsed flags and the operand list
/// (everything from the first non-option token onward). On an unknown flag we
/// call usage(), matching node's `default:` case.
fn parse(argv: &[String]) -> (Opts, Vec<String>) {
    let mut opts = Opts {
        efi: false,
        encrypt: false,
        force: false,
    };

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            i += 1;
            break;
        }
        // A non-option (or bare "-") ends option scanning; getopt leaves it
        // and everything after as operands.
        if !arg.starts_with('-') || arg == "-" {
            break;
        }
        for ch in arg[1..].chars() {
            match ch {
                'B' => opts.efi = true,
                'e' => opts.encrypt = true,
                'f' => opts.force = true,
                _ => usage(),
            }
        }
        i += 1;
    }

    (opts, argv[i..].to_vec())
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let (opts, operands) = parse(&argv);

    let pool = match operands.first() {
        Some(p) => p,
        None => usage(),
    };
    let file = match operands.get(1) {
        Some(f) => f,
        None => usage(),
    };

    let json = match fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) => fatal(&format!("{file}: {e}")),
    };
    let config: layout::Config = match serde_json::from_str(&json) {
        Ok(c) => c,
        Err(e) => fatal(&format!("invalid disklayout JSON: {e}")),
    };

    let cmd = layout::build_command(pool, &config, opts.force, opts.encrypt, opts.efi);

    if std::env::var_os("MKZPOOL_DRY_RUN").is_some() {
        let mut line = String::from(&cmd.program);
        for a in &cmd.args {
            line.push(' ');
            line.push_str(a);
        }
        println!("{line}");
        return;
    }

    // node's zfs.js runs this via execFile and, on failure, reports stderr as
    // the error. Capture output so we can reproduce that "fatal error: pool
    // creation failed: <stderr>" behaviour.
    let out = match Command::new(&cmd.program).args(&cmd.args).output() {
        Ok(o) => o,
        Err(e) => fatal(&format!("pool creation failed: {} {e}", cmd.program)),
    };
    if !out.status.success() {
        fatal(&format!(
            "pool creation failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
}
