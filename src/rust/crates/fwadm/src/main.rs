// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `fwadm` CLI: native Rust drop-in for the SmartOS fwadm command. Output is
//! matched to the node CLI (`src/fw/lib/{fwadm.js,cli.js}`).

use std::io::Read;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{json, Value};

use fwadm::apply::{self, IpfCtl, StatusVal};
use fwadm::error::FwError;
use fwadm::ipf::{prepare_ipf_data, CompileOpts};
use fwadm::model::{FwRule, RuleData};
use fwadm::parse::ParseOpts;
use fwadm::store::{self, Store};
use fwadm::vm::VmLookup;
use fwadm::vmsource;

/// Default list fields.
const DEFAULT_FIELDS: &[&str] = &["uuid", "enabled", "rule"];

/// Parsed common options.
#[derive(Default, Clone)]
struct Opts {
    json: bool,
    verbose: bool,
    parseable: bool,
    delim: Option<String>,
    fields: Option<Vec<String>>,
    enable: bool,
    global: bool,
    owner_uuid: Option<String>,
    file: Option<String>,
    description: Option<String>,
    dryrun: bool,
    stdout: bool,
    store_base: Option<String>,
    date: Option<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    match run(&argv) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliExit::Usage(e)) => {
            eprintln!("{e}");
            ExitCode::from(2)
        }
        Err(CliExit::Runtime(e)) => {
            for line in e.leaf_messages() {
                eprintln!("{line}");
            }
            ExitCode::from(1)
        }
    }
}

enum CliExit {
    Usage(String),
    Runtime(FwError),
}

impl From<FwError> for CliExit {
    fn from(e: FwError) -> Self {
        CliExit::Runtime(e)
    }
}

impl From<serde_json::Error> for CliExit {
    fn from(e: serde_json::Error) -> Self {
        CliExit::Runtime(FwError::generic(e.to_string()))
    }
}

fn run(argv: &[String]) -> Result<(), CliExit> {
    if argv.len() < 2 {
        return Err(CliExit::Usage(usage()));
    }

    // Separate the subcommand from flags. We accept flags before/after the
    // subcommand like the node CLI does via cmdln.
    let mut args = argv[1..].to_vec();

    // Pull out a leading global help.
    if args.iter().any(|a| a == "-h" || a == "--help") && first_nonflag(&args).is_none()
    {
        println!("{}", usage());
        return Ok(());
    }

    let cmd = match first_nonflag(&args) {
        Some(i) => args.remove(i),
        None => return Err(CliExit::Usage(usage())),
    };

    let (opts, positionals) = parse_opts(&args)?;

    match cmd.as_str() {
        "list" => cmd_list(&opts),
        "get" => cmd_get(&opts, &positionals),
        "add" => cmd_add(&opts, &positionals),
        "update" => cmd_update(&opts, &positionals),
        "delete" => cmd_delete(&opts, &positionals),
        "enable" => cmd_enable_disable(&opts, &positionals, true),
        "disable" => cmd_enable_disable(&opts, &positionals, false),
        "vms" => cmd_vms(&opts, &positionals),
        "rules" => cmd_rules(&opts, &positionals),
        "start" => cmd_start_stop(&opts, &positionals, true),
        "stop" => cmd_start_stop(&opts, &positionals, false),
        "status" => cmd_status(&opts, &positionals),
        "stats" => cmd_stats(&opts, &positionals),
        // Internal diagnostic (not in node's surface): parse a rule string and
        // print its canonical text (and serialized JSON with -j). Used by the
        // DSL round-trip differential harness against node fwrule.
        "_canon" => cmd_canon(&opts, &positionals),
        // Internal diagnostic: compile + print the per-zone ipf.conf/ipf6.conf
        // for a VM to stdout WITHOUT writing or applying. Used by the ipf
        // compiler differential harness. Reads rules from --store (or /var/fw)
        // and VMs from vminfod.
        "_compile" => cmd_compile(&opts, &positionals),
        "help" | "?" => {
            println!("{}", usage());
            Ok(())
        }
        other => Err(CliExit::Usage(format!(
            "fwadm: unknown command \"{other}\""
        ))),
    }
}

fn first_nonflag(args: &[String]) -> Option<usize> {
    args.iter().position(|a| !a.starts_with('-'))
}

/// Parse the common option set; remaining tokens are positionals.
fn parse_opts(args: &[String]) -> Result<(Opts, Vec<String>), CliExit> {
    let mut o = Opts::default();
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let take_val = |i: &mut usize| -> Result<String, CliExit> {
            *i += 1;
            args.get(*i)
                .cloned()
                .ok_or_else(|| CliExit::Usage(format!("missing value for {a}")))
        };
        match a.as_str() {
            "-j" | "--json" => o.json = true,
            "-v" | "--verbose" => o.verbose = true,
            "-p" | "--parseable" => o.parseable = true,
            "-e" | "--enable" => o.enable = true,
            "-g" | "--global" => o.global = true,
            "--dryrun" => o.dryrun = true,
            "--stdout" => o.stdout = true,
            "-h" | "--help" => {}
            "-d" | "--delim" => o.delim = Some(take_val(&mut i)?),
            "-o" | "--fields" => {
                let v = take_val(&mut i)?;
                o.fields = Some(v.split(',').map(String::from).collect());
            }
            "-O" | "--owner_uuid" => o.owner_uuid = Some(take_val(&mut i)?),
            "-f" | "--file" => o.file = Some(take_val(&mut i)?),
            "--desc" | "--description" => {
                o.description = Some(take_val(&mut i)?)
            }
            "--store" => o.store_base = Some(take_val(&mut i)?),
            "--date" => o.date = Some(take_val(&mut i)?),
            other if other.starts_with('-') => {
                return Err(CliExit::Usage(format!("unknown option {other}")));
            }
            _ => pos.push(a.clone()),
        }
        i += 1;
    }
    Ok((o, pos))
}

fn store_for(opts: &Opts) -> Store {
    match &opts.store_base {
        Some(b) => Store::new(b),
        None => Store::default(),
    }
}

fn parse_opts_for_store() -> ParseOpts {
    ParseOpts::default()
}

// --- list ---

fn cmd_list(opts: &Opts) -> Result<(), CliExit> {
    if opts.delim.is_some() && !opts.parseable {
        return Err(CliExit::Usage("-d requires -p".into()));
    }
    if opts.json && opts.parseable {
        return Err(CliExit::Usage("cannot specify both -j and -p".into()));
    }

    let store = store_for(opts);
    let _lock = store.lock(false)?;
    let mut rules = store.load_all_rules(parse_opts_for_store())?;
    // node returns rules in readdir order; sort by uuid for determinism is NOT
    // what node does. Preserve filesystem order to match. read_dir order is
    // unspecified, so sort by uuid to be stable; node also has no guaranteed
    // order, so the parity check compares as sets where order differs.
    rules.sort_by(|a, b| a.uuid.cmp(&b.uuid));

    if opts.json {
        let arr: Vec<Value> = rules.iter().map(|r| r.serialize()).collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
        return Ok(());
    }

    let fields: Vec<String> = opts
        .fields
        .clone()
        .unwrap_or_else(|| DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect());

    display_table(&rules, &fields, opts);
    Ok(())
}

/// Render the list table (table or parseable), matching cli.js displayRules.
fn display_table(rules: &[FwRule], fields: &[String], opts: &Opts) {
    let default_widths = |f: &str| -> usize {
        match f {
            "created_by" => 10,
            "description" => 15,
            "enabled" => 7,
            "global" => 6,
            "owner_uuid" => 36,
            "rule" => 20,
            "uuid" => 36,
            "version" => 20,
            _ => 10,
        }
    };

    let field_value = |r: &FwRule, f: &str| -> Option<String> {
        let v = r.serialize();
        match f {
            "rule" => Some(r.text()),
            "enabled" => Some(r.enabled.to_string()),
            "global" => {
                if r.global {
                    Some("true".to_string())
                } else {
                    None
                }
            }
            _ => v.get(f).map(value_to_field_string),
        }
    };

    if opts.parseable {
        let delim = opts.delim.clone().unwrap_or_else(|| ":".to_string());
        for r in rules {
            let cols: Vec<String> = fields
                .iter()
                .map(|f| {
                    let raw = field_value(r, f).unwrap_or_else(|| "-".to_string());
                    raw.replace(&delim, &format!("\\{delim}"))
                })
                .collect();
            println!("{}", cols.join(&delim));
        }
        return;
    }

    // Table: compute widths from header + values.
    let mut widths: Vec<usize> = fields
        .iter()
        .map(|f| default_widths(f).max(f.len()))
        .collect();
    let mut rows: Vec<Vec<String>> = Vec::new();
    for r in rules {
        let row: Vec<String> = fields
            .iter()
            .map(|f| field_value(r, f).unwrap_or_else(|| "-".to_string()))
            .collect();
        for (i, c) in row.iter().enumerate() {
            if c.len() > widths[i] {
                widths[i] = c.len();
            }
        }
        rows.push(row);
    }

    // Header. node's `tab` pads every column (including the last) to its
    // width and joins with a single space, leaving trailing whitespace.
    let header: Vec<String> = fields
        .iter()
        .enumerate()
        .map(|(i, f)| pad(&f.to_uppercase(), widths[i]))
        .collect();
    println!("{}", header.join(" "));
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(i, c)| pad(c, widths[i]))
            .collect();
        println!("{}", line.join(" "));
    }
}

fn pad(s: &str, w: usize) -> String {
    if s.len() >= w {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(w - s.len()))
    }
}

fn value_to_field_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => "-".to_string(),
        other => other.to_string(),
    }
}

// --- get ---

fn cmd_get(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let store = store_for(opts);
    let _lock = store.lock(false)?;
    let rule = store.load_rule(&uuid, parse_opts_for_store())?;
    println!("{}", serde_json::to_string_pretty(&rule.serialize())?);
    Ok(())
}

// --- add ---

fn cmd_add(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let payloads = collect_rule_payloads(opts, pos)?;
    let store = store_for(opts);

    let parse_o = parse_opts_for_store();
    let mut new_rules = Vec::new();
    for data in payloads {
        new_rules.push(FwRule::create(data, parse_o)?);
    }

    if !opts.dryrun {
        let _lock = store.lock(true)?;
        store.save_rules(&new_rules)?;
        apply_after_change(opts, &store)?;
    }

    output_rules("Added", &new_rules, opts, &store)?;
    Ok(())
}

// --- update ---

fn cmd_update(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    // `update <uuid> <rule...>` or `update -f file`.
    let mut pos = pos.to_vec();
    let mut id: Option<String> = None;
    if !pos.is_empty() && fwadm::model::is_uuid(&pos[0]) {
        // Could be a leading uuid; node shifts the first arg as id.
        id = Some(pos.remove(0));
    }

    let mut payloads = collect_rule_payloads(opts, &pos)?;
    if let Some(uuid) = &id {
        if payloads.len() == 1 {
            payloads[0].uuid = Some(uuid.clone());
        }
    }

    let store = store_for(opts);
    let parse_o = parse_opts_for_store();

    let _lock = store.lock(true)?;
    let existing = store.load_all_rules(parse_o)?;
    let mut updated = Vec::new();
    for mut data in payloads {
        // Merge with existing rule for fields not provided.
        if let Some(uuid) = &data.uuid {
            if let Some(prev) = existing.iter().find(|r| &r.uuid == uuid) {
                if data.rule.is_none() {
                    data.rule = Some(prev.text());
                }
                if data.enabled.is_none() {
                    data.enabled = Some(prev.enabled);
                }
                if data.owner_uuid.is_none() {
                    data.owner_uuid = prev.owner_uuid.clone();
                }
                if data.description.is_none() {
                    data.description = prev.description.clone();
                }
                if data.version.is_none() {
                    data.version = Some(store::generate_version());
                }
            }
        }
        updated.push(FwRule::create(data, parse_o)?);
    }

    if !opts.dryrun {
        store.save_rules(&updated)?;
        apply_after_change(opts, &store)?;
    }
    drop(_lock);

    output_rules("Updated", &updated, opts, &store)?;
    Ok(())
}

// --- delete ---

fn cmd_delete(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    if pos.is_empty() {
        eprintln!("Must specify rules to delete!");
        return Ok(());
    }
    for u in pos {
        if !fwadm::model::is_uuid(u) {
            eprintln!("Error: invalid UUID \"{u}\"");
            return Err(CliExit::Runtime(FwError::generic("invalid uuid")));
        }
    }
    let store = store_for(opts);
    let parse_o = parse_opts_for_store();

    let _lock = store.lock(true)?;
    // Load the rules being deleted so we can report them.
    let mut deleted = Vec::new();
    for u in pos {
        if let Ok(r) = store.load_rule(u, parse_o) {
            deleted.push(r);
        }
    }
    store.delete_rules(pos)?;
    if !opts.dryrun {
        apply_after_change(opts, &store)?;
    }
    drop(_lock);

    output_rules("Deleted", &deleted, opts, &store)?;
    Ok(())
}

// --- enable / disable ---

fn cmd_enable_disable(
    opts: &Opts,
    pos: &[String],
    enable: bool,
) -> Result<(), CliExit> {
    if pos.is_empty() {
        let verb = if enable { "enable" } else { "disable" };
        return Err(CliExit::Usage(format!("Must specify rules to {verb}!")));
    }
    let store = store_for(opts);
    let parse_o = parse_opts_for_store();

    let _lock = store.lock(true)?;
    let mut changed = Vec::new();
    for u in pos {
        if !fwadm::model::is_uuid(u) {
            eprintln!("Error: invalid UUID \"{u}\"");
            return Err(CliExit::Runtime(FwError::generic("invalid uuid")));
        }
        let prev = store.load_rule(u, parse_o)?;
        let data = RuleData {
            rule: Some(prev.text()),
            uuid: Some(prev.uuid.clone()),
            version: Some(store::generate_version()),
            owner_uuid: prev.owner_uuid.clone(),
            enabled: Some(enable),
            log: Some(prev.log),
            description: prev.description.clone(),
            created_by: prev.created_by.clone(),
            parsed: None,
        };
        changed.push(FwRule::create(data, parse_o)?);
    }
    store.save_rules(&changed)?;
    if !opts.dryrun {
        apply_after_change(opts, &store)?;
    }
    drop(_lock);

    let action = if enable { "Enabled" } else { "Disabled" };
    output_rules(action, &changed, opts, &store)?;
    Ok(())
}

// --- vms (rule -> affected VM uuids) ---

fn cmd_vms(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let store = store_for(opts);
    let parse_o = parse_opts_for_store();
    let _lock = store.lock(false)?;
    let rule = store.load_rule(&uuid, parse_o)?;
    drop(_lock);

    let vms = vmsource::load_vms()?;
    let lk = VmLookup::build(vms);
    let mut matched = fwadm::ipf::vms_affected(&lk, &rule);
    matched.sort();
    matched.dedup();

    if opts.json {
        println!("{}", serde_json::to_string_pretty(&json!(matched))?);
    } else {
        println!("{}", matched.join("\n"));
    }
    Ok(())
}

// --- rules (vm -> applicable rules) ---

fn cmd_rules(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let store = store_for(opts);
    let parse_o = parse_opts_for_store();
    let _lock = store.lock(false)?;
    let all = store.load_all_rules(parse_o)?;
    drop(_lock);

    let vms = vmsource::load_vms()?;
    let lk = VmLookup::build(vms);

    let mut applicable: Vec<FwRule> = all
        .into_iter()
        .filter(|r| fwadm::ipf::vms_affected(&lk, r).iter().any(|u| u == &uuid))
        .collect();
    applicable.sort_by(|a, b| a.uuid.cmp(&b.uuid));

    if opts.json {
        let arr: Vec<Value> = applicable.iter().map(|r| r.serialize()).collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    } else {
        let fields: Vec<String> = opts.fields.clone().unwrap_or_else(|| {
            DEFAULT_FIELDS.iter().map(|s| s.to_string()).collect()
        });
        display_table(&applicable, &fields, opts);
    }
    Ok(())
}

// --- start / stop ---

fn cmd_start_stop(
    opts: &Opts,
    pos: &[String],
    enable: bool,
) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    // Toggling firewall_enabled requires VM.update (vmadm); out of scope for a
    // read/compile port. We surface the intended action; the apply happens via
    // vmadm in the integrated platform.
    let _ = enable;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&json!({"result":"success"}))?);
    } else {
        println!(
            "Firewall {} for VM {}",
            if enable { "started" } else { "stopped" },
            uuid
        );
    }
    Err(CliExit::Runtime(FwError::generic(
        "start/stop requires vmadm VM.update integration (not applied)",
    )))
}

// --- status ---

fn cmd_status(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let ctl = IpfCtl::default();
    let res = apply::status(ctl, &uuid)?;

    if opts.json {
        let mut m = serde_json::Map::new();
        for (k, v) in &res {
            m.insert(k.clone(), status_val_json(v));
        }
        println!("{}", serde_json::to_string_pretty(&Value::Object(m))?);
        return Ok(());
    }
    if opts.verbose {
        for (k, v) in &res {
            println!("{k}: {}", status_val_str(v));
        }
        return Ok(());
    }
    let running = res
        .iter()
        .find(|(k, _)| k == "running")
        .map(|(_, v)| matches!(v, StatusVal::Bool(true)))
        .unwrap_or(false);
    println!("{}", if running { "running" } else { "stopped" });
    Ok(())
}

fn status_val_json(v: &StatusVal) -> Value {
    match v {
        StatusVal::Str(s) => Value::String(s.clone()),
        StatusVal::Bool(b) => Value::Bool(*b),
        StatusVal::Num(n) => Value::Number((*n).into()),
    }
}

fn status_val_str(v: &StatusVal) -> String {
    match v {
        StatusVal::Str(s) => s.clone(),
        StatusVal::Bool(b) => b.to_string(),
        StatusVal::Num(n) => n.to_string(),
    }
}

// --- stats ---

fn cmd_stats(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let ctl = IpfCtl::default();
    let res = apply::rule_stats(ctl, &uuid)?;
    if opts.json {
        let arr: Vec<Value> = res
            .iter()
            .map(|(h, r)| json!({"hits": h, "rule": r}))
            .collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    } else {
        for (h, r) in &res {
            println!("{h} {r}");
        }
    }
    Ok(())
}

// --- internal diagnostic: canonicalize a rule string ---

fn cmd_canon(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let text = pos.join(" ");
    let data = RuleData {
        rule: Some(text),
        uuid: Some("00000000-0000-0000-0000-000000000000".to_string()),
        version: Some("0.000000".to_string()),
        ..Default::default()
    };
    let rule = FwRule::create(data, parse_opts_for_store())?;
    if opts.json {
        println!("{}", serde_json::to_string_pretty(&rule.serialize())?);
    } else {
        println!("{}", rule.text());
    }
    Ok(())
}

fn cmd_compile(opts: &Opts, pos: &[String]) -> Result<(), CliExit> {
    let uuid = require_uuid(pos.first())?;
    let store = store_for(opts);
    let parse_o = parse_opts_for_store();
    let rules = store.load_all_rules(parse_o)?;

    // VM source: a JSON file (-f) of either a single VM or an array, else
    // vminfod.
    let vms = if let Some(file) = &opts.file {
        let raw = std::fs::read_to_string(file)
            .map_err(|e| CliExit::Runtime(FwError::generic(e.to_string())))?;
        let v: Value = serde_json::from_str(&raw)?;
        let arr = if v.is_array() {
            v.as_array().cloned().unwrap_or_default()
        } else {
            vec![v]
        };
        arr.iter().filter_map(fwadm::vm::Vm::from_json).collect()
    } else {
        vmsource::load_vms()?
    };

    let lk = VmLookup::build(vms.clone());
    let have_ipfev = Path::new("/dev/ipfev").exists();
    let date = opts
        .date
        .clone()
        .unwrap_or_else(|| "FIXED-DATE".to_string());

    let target_vm: Vec<_> = vms.iter().filter(|v| v.uuid == uuid).cloned().collect();
    let compile = CompileOpts {
        all_vms: &lk,
        rules: &rules,
        vms: &target_vm,
        have_ipfev,
        date,
    };
    let confs = prepare_ipf_data(&compile);
    for c in &confs {
        if c.uuid != uuid {
            continue;
        }
        let v4 = Path::new(&c.zonepath).join("config/ipf.conf");
        let v6 = Path::new(&c.zonepath).join("config/ipf6.conf");
        println!("=== {}", v4.display());
        print!("{}", c.v4text);
        println!();
        println!("=== {}", v6.display());
        print!("{}", c.v6text);
    }
    Ok(())
}

// --- shared helpers ---

fn require_uuid(arg: Option<&String>) -> Result<String, CliExit> {
    match arg {
        None => {
            eprintln!("Error: missing UUID");
            Err(CliExit::Runtime(FwError::generic("missing UUID")))
        }
        Some(u) if fwadm::model::is_uuid(u) => Ok(u.clone()),
        Some(u) => {
            eprintln!("Error: invalid UUID \"{u}\"");
            Err(CliExit::Runtime(FwError::generic("invalid UUID")))
        }
    }
}

/// Build RuleData payloads from CLI args (text) or a JSON file/stdin.
fn collect_rule_payloads(
    opts: &Opts,
    pos: &[String],
) -> Result<Vec<RuleData>, CliExit> {
    // From args (rule text).
    if opts.file.is_none() && !pos.is_empty() {
        let mut d = RuleData {
            rule: Some(pos.join(" ")),
            ..Default::default()
        };
        apply_flag_fields(opts, &mut d);
        return Ok(vec![d]);
    }

    // From file / stdin JSON.
    let raw = read_payload_source(opts)?;
    let v: Value = serde_json::from_str(&raw)
        .map_err(|e| CliExit::Runtime(FwError::generic(e.to_string())))?;

    let rule_objs: Vec<Value> = if let Some(rules) =
        v.get("rules").and_then(|r| r.as_array())
    {
        rules.clone()
    } else {
        vec![v.clone()]
    };

    let mut out = Vec::new();
    for obj in rule_objs {
        let mut d = json_to_ruledata(&obj);
        apply_flag_fields(opts, &mut d);
        out.push(d);
    }
    Ok(out)
}

fn read_payload_source(opts: &Opts) -> Result<String, CliExit> {
    let file = match &opts.file {
        Some(f) => f.clone(),
        None => "-".to_string(),
    };
    if file == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .map_err(|e| CliExit::Runtime(FwError::generic(e.to_string())))?;
        Ok(s)
    } else {
        if !Path::new(&file).exists() {
            return Err(CliExit::Runtime(FwError::generic(format!(
                "File \"{file}\" does not exist."
            ))));
        }
        std::fs::read_to_string(&file)
            .map_err(|e| CliExit::Runtime(FwError::generic(e.to_string())))
    }
}

fn json_to_ruledata(obj: &Value) -> RuleData {
    let s = |k: &str| obj.get(k).and_then(|x| x.as_str()).map(String::from);
    let b = |k: &str| obj.get(k).and_then(|x| x.as_bool());
    RuleData {
        rule: s("rule"),
        parsed: None,
        uuid: s("uuid"),
        version: s("version"),
        owner_uuid: s("owner_uuid"),
        enabled: b("enabled"),
        log: b("log"),
        created_by: s("created_by"),
        description: s("description"),
    }
}

fn apply_flag_fields(opts: &Opts, d: &mut RuleData) {
    if opts.enable {
        d.enabled = Some(true);
    }
    if let Some(o) = &opts.owner_uuid {
        d.owner_uuid = Some(o.clone());
    }
    if let Some(desc) = &opts.description {
        d.description = Some(desc.clone());
    }
    // -g (global) means "no owner": represented by leaving owner_uuid None,
    // which the model treats as global. If both -g and -O are given, node lets
    // owner_uuid win unless enforceGlobal; local CLI does not enforce.
    if opts.global && opts.owner_uuid.is_none() {
        d.owner_uuid = None;
    }
}

/// After a store mutation, recompile and apply ipf for affected VMs (unless
/// dryrun). On a verification store (custom --store), apply is skipped.
fn apply_after_change(opts: &Opts, store: &Store) -> Result<(), CliExit> {
    if opts.store_base.is_some() {
        // Verification store: never touch live ipf.
        return Ok(());
    }
    let parse_o = parse_opts_for_store();
    let rules = store.load_all_rules(parse_o)?;
    let vms = vmsource::load_vms()?;
    let lk = VmLookup::build(vms.clone());
    let have_ipfev = Path::new("/dev/ipfev").exists();
    let date = chrono::Local::now().format("%a %b %e %Y %T GMT%z (%Z)").to_string();

    let compile = CompileOpts {
        all_vms: &lk,
        rules: &rules,
        vms: &vms,
        have_ipfev,
        date,
    };
    let confs = prepare_ipf_data(&compile);
    let ctl = IpfCtl::default();
    for conf in &confs {
        apply::write_confs(conf)?;
        let v4 = Path::new(&conf.zonepath).join("config/ipf.conf");
        let v6 = Path::new(&conf.zonepath).join("config/ipf6.conf");
        apply::reload(ctl, &conf.uuid, &v4, &v6)?;
    }
    Ok(())
}

/// Print the rule-output block (Added/Updated/... + optional VMs affected /
/// --stdout file dump), matching ruleOutput().
fn output_rules(
    action: &str,
    rules: &[FwRule],
    opts: &Opts,
    _store: &Store,
) -> Result<(), CliExit> {
    if opts.json {
        let arr: Vec<Value> = rules.iter().map(|r| r.serialize()).collect();
        let res = json!({ "rules": arr });
        println!("{}", serde_json::to_string_pretty(&res)?);
        return Ok(());
    }

    let mut out: Vec<String> = Vec::new();
    if !rules.is_empty() {
        out.push(format!("{action} rules:"));
        for r in rules {
            out.push(rule_line(r));
        }
    }
    println!("{}", out.join("\n"));
    Ok(())
}

/// Matches cli.js ruleLine: "<uuid> <enabled-padded> <rule>".
fn rule_line(r: &FwRule) -> String {
    let enabled = if r.enabled { "true   " } else { "false  " };
    format!("{} {} {}", r.uuid, enabled, r.text())
}

fn usage() -> String {
    "Usage: fwadm [-hjv] COMMAND [ARGS]\n\n\
     Commands:\n    \
     add, delete, disable, enable, get, list, update, vms\n    \
     rules, start, status, stats, stop\n"
        .to_string()
}
