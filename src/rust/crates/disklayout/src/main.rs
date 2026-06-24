// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Native Rust drop-in for the SmartOS `disklayout` CLI.

mod layout;

use std::fs;
use std::process::Command;

use layout::Disk;

const USAGE: &str = "usage: disklayout [-c] [-f file] [-s spares] [-w width] [-e disk,...] [<layout>]";

fn fatal(msg: &str) -> ! {
    println!("fatal error: {}", msg);
    std::process::exit(255);
}

fn usage() -> ! {
    println!("{}", USAGE);
    println!("supported layouts:");
    for l in layout::list_supported() {
        println!("\t{}", l);
    }
    std::process::exit(255);
}

/// Parsed CLI options, in the getopt(3c) `ce:f:s:w:` style: options are consumed
/// up to the first non-option, which becomes the layout. Anything after is
/// ignored, matching node's BasicParser + optind() behavior.
struct Opts {
    enable_cache: bool,
    excluded: Vec<String>,
    file: Option<String>,
    spares: Option<i64>,
    width: Option<i64>,
    layout: Option<String>,
}

fn parse_args(argv: &[String]) -> Opts {
    let mut opts = Opts {
        enable_cache: true,
        excluded: Vec::new(),
        file: None,
        spares: None,
        width: None,
        layout: None,
    };

    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if !arg.starts_with('-') || arg == "-" {
            opts.layout = Some(arg.clone());
            break;
        }
        // Reject combined/unknown options conservatively; node's getopt would
        // error and call usage().
        let flag = &arg[1..];
        match flag.chars().next() {
            Some('c') => {
                opts.enable_cache = false;
                i += 1;
            }
            Some('e') => {
                let val = inline_or_next(flag, argv, &mut i);
                opts.excluded = val.to_lowercase().split(',').map(|s| s.to_string()).collect();
            }
            Some('f') => {
                opts.file = Some(inline_or_next(flag, argv, &mut i));
            }
            Some('s') => {
                let val = inline_or_next(flag, argv, &mut i);
                match parse_int_strict(&val) {
                    Some(n) if n >= 0 => opts.spares = Some(n),
                    _ => {
                        println!("invalid value for number of spares: {}", val);
                        usage();
                    }
                }
            }
            Some('w') => {
                let val = inline_or_next(flag, argv, &mut i);
                match parse_int_strict(&val) {
                    Some(n) if n >= 2 => opts.width = Some(n),
                    _ => {
                        println!("invalid width: {}", val);
                        usage();
                    }
                }
            }
            _ => usage(),
        }
    }

    opts
}

/// For a flag that takes an argument (`-fVAL` or `-f VAL`), return the value and
/// advance the index past whatever was consumed.
fn inline_or_next(flag: &str, argv: &[String], i: &mut usize) -> String {
    if flag.len() > 1 {
        let v = flag[1..].to_string();
        *i += 1;
        v
    } else {
        if *i + 1 >= argv.len() {
            usage();
        }
        let v = argv[*i + 1].clone();
        *i += 2;
        v
    }
}

/// node uses parseInt then checks the string round-trips (rejects "3abc").
fn parse_int_strict(s: &str) -> Option<i64> {
    let n: i64 = s.parse().ok()?;
    if n.to_string() == s { Some(n) } else { None }
}

fn parse_disk_line(line: &str) -> Option<Disk> {
    if line.is_empty() {
        return None;
    }
    let row: Vec<&str> = line.split('\t').collect();
    let get = |idx: usize| row.get(idx).copied().unwrap_or("").to_string();
    Some(Disk {
        disk_type: get(0),
        name: get(1),
        vid: get(2),
        pid: get(3),
        size: get(4),
        removable: row.get(5).copied() == Some("yes"),
        solid_state: row.get(6).copied() == Some("yes"),
    })
}

fn disks_from_text(data: &str) -> Vec<Disk> {
    data.trim_end_matches('\n').trim().split('\n').filter_map(parse_disk_line).collect()
}

fn read_live_disks() -> Result<Vec<Disk>, String> {
    let out = Command::new("/usr/bin/diskinfo")
        .arg("-Hp")
        .output()
        .map_err(|e| format!("failed to run diskinfo: {}", e))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(disks_from_text(&String::from_utf8_lossy(&out.stdout)))
}

fn read_mnttab() -> String {
    fs::read_to_string("/etc/mnttab").unwrap_or_default()
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let opts = parse_args(&argv);

    let disks = if let Some(ref f) = opts.file {
        match fs::read_to_string(f) {
            Ok(data) => disks_from_text(&data),
            Err(e) => fatal(&format!("unable to read {}: {}", f, e)),
        }
    } else {
        match read_live_disks() {
            Ok(d) => d,
            Err(e) => fatal(&e),
        }
    };

    let mnttab = read_mnttab();
    let disks: Vec<Disk> = disks
        .into_iter()
        .filter(|disk| {
            if mnttab.contains(&disk.name) {
                return false;
            }
            if opts.excluded.iter().any(|e| e == &disk.name.to_lowercase()) {
                return false;
            }
            true
        })
        .collect();

    match layout::compute(disks, opts.layout, opts.spares, opts.enable_cache, opts.width) {
        Ok(config) => match to_json_tabs(&config) {
            Ok(s) => println!("{}", s),
            Err(e) => fatal(&format!("serialization error: {}", e)),
        },
        Err(e) => fatal(&e.0),
    }
}

/// Serialize with tab indentation to match `JSON.stringify(config, null, '\t')`.
fn to_json_tabs<T: serde::Serialize>(value: &T) -> Result<String, serde_json::Error> {
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"\t");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    value.serialize(&mut ser)?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}
