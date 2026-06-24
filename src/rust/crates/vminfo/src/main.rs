// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vminfo` — native Rust drop-in for the SmartOS vminfo CLI.
//!
//! Subcommands `ping | status [-jf] | vms | vm <uuid> | events [-jfr]` over
//! an HTTP client to the vminfod daemon (127.0.0.1:9090). Output is matched
//! byte-for-byte to the node CLI (`src/vm/sbin/vminfo.js`): `status`'s
//! formatted block, `-j`/`vms`/`vm` as `JSON.stringify(msg, null, 2)`, and
//! `events` as the per-line trace.

mod client;
mod events;
mod pretty;
mod status;

use std::process::ExitCode;

use clap::{Arg, ArgAction, Command};

use crate::client::{Client, ClientError};

/// Default daemon endpoint (node binds 127.0.0.1:9090). Overridable via the
/// `--port`/`--host` flags for differential testing against an alternate
/// daemon, and via `VMINFO_PORT`/`VMINFO_HOST` env vars.
const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: u16 = 9090;

fn user_agent() -> String {
    // node makeUserAgent: "<name> - <host>/<pid> (<argv1>)". The CLI name is
    // "Vminfod CLI"; reproduce a compatible shape for /status eventsListeners.
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    format!("Vminfod CLI - {host}/{} (vminfo)", std::process::id())
}

fn main() -> ExitCode {
    let matches = cli().get_matches();

    let host = std::env::var("VMINFO_HOST").unwrap_or_else(|_| DEFAULT_HOST.to_string());
    let port = std::env::var("VMINFO_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_PORT);
    let client = Client::new(host, port, user_agent());

    match matches.subcommand() {
        Some(("ping", _)) => do_ping(&client),
        Some(("status", m)) => {
            status::do_status(&client, m.get_flag("full"), m.get_flag("json"))
        }
        Some(("vms", _)) => do_vms(&client),
        Some(("vm", m)) => {
            let uuid = m.get_one::<String>("uuid").map(String::as_str).unwrap_or("");
            do_vm(&client, uuid)
        }
        Some(("events", m)) => events::do_events(
            &client,
            m.get_flag("full"),
            m.get_flag("json"),
            m.get_flag("ready"),
        ),
        _ => {
            usage(None);
            ExitCode::FAILURE
        }
    }
}

fn cli() -> Command {
    Command::new("vminfo")
        .about("Vminfod CLI")
        .disable_help_subcommand(true)
        .subcommand_required(false)
        .arg_required_else_help(false)
        .subcommand(Command::new("ping").about("get vminfod ping     (GET /ping)"))
        .subcommand(
            Command::new("status")
                .about("get vminfod status   (GET /status)")
                .arg(Arg::new("full").short('f').long("full").action(ArgAction::SetTrue))
                .arg(Arg::new("json").short('j').long("json").action(ArgAction::SetTrue)),
        )
        .subcommand(Command::new("vms").about("get all vms          (GET /vms)"))
        .subcommand(
            Command::new("vm")
                .about("get vm info          (GET /vms/:uuid)")
                .arg(Arg::new("uuid").required(true)),
        )
        .subcommand(
            Command::new("events")
                .about("trace vminfod events (GET /events)")
                .arg(Arg::new("full").short('f').long("full").action(ArgAction::SetTrue))
                .arg(Arg::new("json").short('j').long("json").action(ArgAction::SetTrue))
                .arg(Arg::new("ready").short('r').long("ready").action(ArgAction::SetTrue)),
        )
}

/// Print the node usage block (matched to vminfo.js `usage()`).
pub fn usage(msg: Option<&str>) {
    let mut out = Vec::new();
    if let Some(m) = msg {
        out.push(m.to_string());
        out.push(String::new());
    }
    out.extend(
        [
            "Usage: vminfo [command] [args]",
            "",
            "Commands",
            "  ping               get vminfod ping     (GET /ping)",
            "  status [-jf]       get vminfod status   (GET /status)",
            "  vms                get all vms          (GET /vms)",
            "  vm <uuid>          get vm info          (GET /vms/:uuid)",
            "  events [-jfr]      trace vminfod events (GET /events)",
        ]
        .iter()
        .map(|s| (*s).to_string()),
    );
    println!("{}", out.join("\n"));
}

/// Print a client error to stderr and exit 1 (node: `console.error(err.message)`).
pub fn fail(err: &ClientError) -> ExitCode {
    eprintln!("{err}");
    ExitCode::FAILURE
}

fn do_ping(client: &Client) -> ExitCode {
    match client.get_json("/ping") {
        Ok(v) => match v.get("ping").and_then(serde_json::Value::as_str) {
            Some(ping) => {
                println!("{ping}");
                ExitCode::SUCCESS
            }
            None => {
                eprintln!("msg.ping (string) is required");
                ExitCode::FAILURE
            }
        },
        Err(e) => fail(&e),
    }
}

fn do_vms(client: &Client) -> ExitCode {
    match client.get_json("/vms") {
        Ok(v) => {
            println!("{}", pretty::stringify_pretty(&v));
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

fn do_vm(client: &Client, uuid: &str) -> ExitCode {
    if uuid.is_empty() {
        usage(Some("vm zonename must be specified"));
        return ExitCode::FAILURE;
    }
    let path = format!("/vms/{}", urlencode(uuid));
    match client.get_json(&path) {
        Ok(v) => {
            println!("{}", pretty::stringify_pretty(&v));
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// Minimal percent-encoding for a path component (node `encodeURIComponent`).
/// uuids never need it, but a stray odd zonename should be encoded rather
/// than break the request line.
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'!' | b'~' | b'*'
            | b'\'' | b'(' | b')' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
