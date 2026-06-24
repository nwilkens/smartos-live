// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `bunyan` — native Rust drop-in for the node-bunyan log pretty-printer.
//! Reads NDJSON bunyan records from stdin or file args and renders them in
//! `long` (default), `short`, `json[-N]`, or `bunyan` output modes, with
//! `-l/--level`, `-c/--condition`, `--pid`, and `--strict` filtering. Matches
//! node-bunyan 1.3.4 output byte-for-byte for these modes.

mod condition;
mod level;
mod render;

use std::env;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::ExitCode;

use anyhow::{Result, anyhow, bail};
use serde_json::{Map, Value};

use condition::Condition;
use render::{OutputMode, RenderOpts};

const VERSION: &str = "1.3.4";

struct Opts {
    mode: OutputMode,
    json_indent: usize,
    color: level::ColorMode,
    level: Option<i64>,
    strict: bool,
    conditions: Vec<Condition>,
    pids: bool,
    files: Vec<String>,
}

fn main() -> ExitCode {
    let argv: Vec<String> = env::args().collect();
    match run(&argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("bunyan: error: {e}");
            ExitCode::from(1)
        }
    }
}

fn run(argv: &[String]) -> Result<ExitCode> {
    let raw = expand_short_clusters(&argv[1..]);
    let parsed = parse(raw)?;

    let parsed = match parsed {
        Parsed::Help => {
            print_help();
            return Ok(ExitCode::SUCCESS);
        }
        Parsed::Version => {
            println!("bunyan {VERSION}");
            return Ok(ExitCode::SUCCESS);
        }
        Parsed::Opts(o) => o,
    };

    let color = resolve_color(parsed.color);
    let ropts = RenderOpts {
        mode: parsed.mode,
        color,
        json_indent: parsed.json_indent,
    };

    let stdout = io::stdout();
    let mut out = io::BufWriter::new(stdout.lock());

    let result = if parsed.pids {
        // DTrace snooping is platform-specific and out of scope for the lift;
        // fail loudly rather than silently misbehave.
        bail!("-p/--pid (DTrace snooping) is not supported by this build");
    } else if parsed.files.is_empty() {
        process_reader(BufReader::new(io::stdin().lock()), &parsed, &ropts, &mut out)
    } else {
        process_files(&parsed, &ropts, &mut out)
    };

    if color {
        // node clears any interrupted ANSI on exit (issue #59).
        let _ = out.write_all(b"\x1b[0m");
    }
    out.flush().ok();
    result?;
    Ok(ExitCode::SUCCESS)
}

fn process_files(opts: &Opts, ropts: &RenderOpts, out: &mut impl Write) -> Result<()> {
    let mut had_err = false;
    for file in &opts.files {
        match File::open(file) {
            Ok(f) => {
                if let Err(e) = process_reader(BufReader::new(f), opts, ropts, out) {
                    eprintln!("bunyan: {e}");
                    had_err = true;
                }
            }
            Err(e) => {
                eprintln!("bunyan: {file}: {e}");
                had_err = true;
            }
        }
    }
    if had_err {
        bail!("one or more files failed");
    }
    Ok(())
}

fn process_reader(
    reader: impl Read,
    opts: &Opts,
    ropts: &RenderOpts,
    out: &mut impl Write,
) -> Result<()> {
    let mut buf = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = buf.read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        // Strip a single trailing \n and optional \r (node splits on \r?\n).
        if line.last() == Some(&b'\n') {
            line.pop();
            if line.last() == Some(&b'\r') {
                line.pop();
            }
        }
        let s = String::from_utf8_lossy(&line);
        handle_line(&s, opts, ropts, out)?;
    }
    Ok(())
}

fn handle_line(line: &str, opts: &Opts, ropts: &RenderOpts, out: &mut impl Write) -> Result<()> {
    if line.is_empty() || !line.starts_with('{') {
        if !opts.strict {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }
    let rec: Map<String, Value> = match serde_json::from_str(line) {
        Ok(Value::Object(m)) => m,
        _ => {
            if !opts.strict {
                writeln!(out, "{line}")?;
            }
            return Ok(());
        }
    };

    if !is_valid_record(&rec) {
        if !opts.strict {
            writeln!(out, "{line}")?;
        }
        return Ok(());
    }

    if !filter_record(&rec, opts) {
        return Ok(());
    }

    let rendered = render::emit_record(&rec, ropts);
    match ropts.mode {
        // long/short already carry node's single trailing newline.
        OutputMode::Long | OutputMode::Short => write!(out, "{rendered}")?,
        OutputMode::Json | OutputMode::Bunyan => writeln!(out, "{rendered}")?,
    }
    Ok(())
}

fn filter_record(rec: &Map<String, Value>, opts: &Opts) -> bool {
    if let Some(min) = opts.level {
        let lvl = rec.get("level").and_then(Value::as_i64).unwrap_or(i64::MIN);
        if lvl < min {
            return false;
        }
    }
    for c in &opts.conditions {
        if !c.eval(rec) {
            return false;
        }
    }
    true
}

fn is_valid_record(rec: &Map<String, Value>) -> bool {
    let present = |k: &str| rec.get(k).is_some_and(|v| !v.is_null());
    present("v")
        && present("level")
        && present("name")
        && present("hostname")
        && present("pid")
        && present("time")
        && present("msg")
}

fn resolve_color(mode: level::ColorMode) -> bool {
    match mode {
        level::ColorMode::On => true,
        level::ColorMode::Off => false,
        level::ColorMode::Auto => {
            if env::var("BUNYAN_NO_COLOR").is_ok_and(|v| !v.is_empty()) {
                false
            } else {
                // SAFETY: isatty has no preconditions beyond a valid fd.
                unsafe { libc::isatty(libc::STDOUT_FILENO) == 1 }
            }
        }
    }
}

enum Parsed {
    Help,
    Version,
    Opts(Box<Opts>),
}

/// Mirror node's pre-pass that splits `-iH` clusters into `-i -H`, stopping at
/// the first arg-taking option in a cluster.
fn expand_short_clusters(args: &[String]) -> Vec<String> {
    let takes_arg = |c: char| matches!(c, 'd' | 'o' | 'c' | 'l' | 'p');
    let mut out = Vec::new();
    for a in args {
        let bytes = a.as_bytes();
        if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' {
            let chars: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < chars.len() {
                out.push(format!("-{}", chars[j]));
                if takes_arg(chars[j]) {
                    let rest: String = chars[j + 1..].iter().collect();
                    if !rest.is_empty() {
                        out.push(rest);
                    }
                    break;
                }
                j += 1;
            }
        } else {
            out.push(a.clone());
        }
    }
    out
}

fn parse(args: Vec<String>) -> Result<Parsed> {
    let mut o = Opts {
        mode: OutputMode::Long,
        json_indent: 2,
        color: level::ColorMode::Auto,
        level: None,
        strict: false,
        conditions: Vec::new(),
        pids: false,
        files: Vec::new(),
    };

    let mut it = args.into_iter().peekable();
    let mut end_of_options = false;
    while let Some(arg) = it.next() {
        if end_of_options {
            o.files.push(arg);
            continue;
        }
        match arg.as_str() {
            "--" => end_of_options = true,
            "-h" | "--help" => return Ok(Parsed::Help),
            "--version" => return Ok(Parsed::Version),
            "--strict" => o.strict = true,
            "--color" => o.color = level::ColorMode::On,
            "--no-color" => o.color = level::ColorMode::Off,
            "--pager" | "--no-pager" => {} // paging unsupported; accept silently
            "-o" | "--output" => {
                let name = it.next().ok_or_else(|| anyhow!("missing argument to \"-o\""))?;
                let (mode, indent) = parse_output_mode(&name)?;
                o.mode = mode;
                if let Some(n) = indent {
                    o.json_indent = n;
                }
            }
            "-j" => o.mode = OutputMode::Json,
            "-0" => o.mode = OutputMode::Bunyan,
            "-L" | "--time" => {
                // Local-time display is accepted but the default (UTC) is
                // preserved; `--time` consumes its argument.
                if arg == "--time" {
                    let _ = it.next().ok_or_else(|| anyhow!("missing argument to \"--time\""))?;
                }
            }
            "-p" | "--pid" => {
                o.pids = true;
                let _ = it.next();
            }
            "-l" | "--level" => {
                let v = it.next().ok_or_else(|| anyhow!("missing argument to \"-l\""))?;
                o.level = Some(parse_level(&v)?);
            }
            "-c" | "--condition" => {
                let v = it.next().ok_or_else(|| anyhow!("missing argument to \"-c\""))?;
                o.conditions.push(Condition::parse(&v)?);
            }
            other => {
                if other.starts_with('-') && other.len() > 1 {
                    bail!("unknown option \"{other}\"");
                }
                o.files.push(arg);
            }
        }
    }
    Ok(Parsed::Opts(Box::new(o)))
}

fn parse_output_mode(name: &str) -> Result<(OutputMode, Option<usize>)> {
    // Trailing `-N` selects JSON indent (e.g. "json-4").
    let (base, indent) = match name.rfind('-') {
        Some(idx) => match name[idx + 1..].parse::<usize>() {
            Ok(n) => (&name[..idx], Some(n)),
            Err(_) => (name, None),
        },
        None => (name, None),
    };
    let mode = match base {
        "long" | "paul" => OutputMode::Long,
        "short" => OutputMode::Short,
        "json" => OutputMode::Json,
        "bunyan" => OutputMode::Bunyan,
        "inspect" | "simple" => bail!("output mode not supported by this build: \"{base}\""),
        _ => bail!("unknown output mode: \"{name}\""),
    };
    Ok((mode, indent))
}

fn parse_level(s: &str) -> Result<i64> {
    if let Ok(n) = s.parse::<i64>() {
        return Ok(n);
    }
    level::level_from_name(&s.to_lowercase())
        .ok_or_else(|| anyhow!("unknown level value: \"{s}\""))
}

fn print_help() {
    let help = "Usage:\n  bunyan [OPTIONS] [FILE ...]\n  ... | bunyan [OPTIONS]\n  bunyan [OPTIONS] -p PID\n\nFilter and pretty-print Bunyan log file content.\n\nGeneral options:\n  -h, --help    print this help info and exit\n  --version     print version of this command and exit\n\nFiltering options:\n  -l, --level LEVEL\n                Only show messages at or above the specified level.\n  -c, --condition CONDITION\n                Run each log message through the condition and\n                only show those that return truish.\n  --strict      Suppress all but legal Bunyan JSON log lines.\n\nOutput options:\n  --color       Colorize output.\n  --no-color    Force no coloring.\n  -o, --output MODE\n                long (default), short, json, json-N, bunyan\n  -j            shortcut for `-o json`\n  -0            shortcut for `-o bunyan`\n\nSee <https://github.com/trentm/node-bunyan> for more complete docs.";
    println!("{help}");
}
