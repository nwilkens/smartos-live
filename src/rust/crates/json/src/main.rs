// json(1) -- a Rust reimplementation of trentm/json 5.1.1, the SmartOS
// command-line JSON tool. Behavior is matched against the shipped node tool.

mod eval;
mod lookup;
mod output;
mod parse_input;

use std::io::{self, Read, Write};
use std::process::ExitCode;

use output::OutputMode;
use parse_input::Merge;
use serde_json::Value;

const VERSION: &str = "5.1.1";

struct Opts {
    help: bool,
    version: bool,
    quiet: bool,
    drop_headers: bool,
    exe_snippets: Vec<String>,
    cond_snippets: Vec<String>,
    output_mode: OutputMode,
    json_indent: usize,
    array: Option<bool>,
    delim: String,
    lookup_delim: char,
    output_keys: bool,
    group: bool,
    merge: Option<Merge>,
    input_files: Vec<String>,
    validate: bool,
    args: Vec<String>,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            help: false,
            version: false,
            quiet: false,
            drop_headers: false,
            exe_snippets: Vec::new(),
            cond_snippets: Vec::new(),
            output_mode: OutputMode::Jsony,
            json_indent: 2,
            array: None,
            delim: " ".to_string(),
            lookup_delim: '.',
            output_keys: false,
            group: false,
            merge: None,
            input_files: Vec::new(),
            validate: false,
            args: Vec::new(),
        }
    }
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let opts = match parse_argv(&argv) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("json: error: {e}");
            return ExitCode::from(1);
        }
    };

    if opts.help {
        print_help();
        return ExitCode::SUCCESS;
    }
    if opts.version {
        println!("json {VERSION}");
        return ExitCode::SUCCESS;
    }

    match run(&opts) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("json: error: {e}");
            ExitCode::from(1)
        }
    }
}

// Mirror json.js parseArgv, including the "-iH" -> "-i -H" splitting with the
// arg-accepting-option exception.
fn parse_argv(argv: &[String]) -> Result<Opts, String> {
    let mut opts = Opts::default();
    let raw: Vec<String> = argv.iter().skip(1).cloned().collect();

    let opt_takes_arg = |c: char| matches!(c, 'd' | 'o' | 'D');

    let mut args: Vec<String> = Vec::new();
    for a in &raw {
        let bytes: Vec<char> = a.chars().collect();
        if bytes.first() == Some(&'-')
            && bytes.get(1) != Some(&'-')
            && bytes.len() > 2
        {
            let split: Vec<char> = bytes[1..].to_vec();
            for (j, &c) in split.iter().enumerate() {
                args.push(format!("-{c}"));
                if opt_takes_arg(c) {
                    let rest: String = split[j + 1..].iter().collect();
                    if !rest.is_empty() {
                        args.push(rest);
                    }
                    break;
                }
            }
        } else {
            args.push(a.clone());
        }
    }

    let mut end_of_options = false;
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--" => end_of_options = true,
            "-h" | "--help" => opts.help = true,
            "--version" => opts.version = true,
            "-q" | "--quiet" => opts.quiet = true,
            "-H" => opts.drop_headers = true,
            "-o" | "--output" => {
                let mut name = it
                    .next()
                    .ok_or("no argument given for '-o|--output' option")?;
                if let Some(idx) = name.rfind('-')
                    && let Ok(indent) = name[idx + 1..].parse::<usize>()
                {
                    opts.json_indent = indent;
                    name = name[..idx].to_string();
                }
                opts.output_mode = match name.as_str() {
                    "jsony" => OutputMode::Jsony,
                    "json" => OutputMode::Json,
                    "inspect" => OutputMode::Inspect,
                    "compact" => OutputMode::Compact,
                    _ => return Err(format!("unknown output mode: '{name}'")),
                };
            }
            "-i" => opts.output_mode = OutputMode::Inspect,
            "-j" => opts.output_mode = OutputMode::Json,
            "-a" | "--array" => opts.array = Some(true),
            "-A" => opts.array = Some(false),
            "-d" => {
                let d = it.next().unwrap_or_default();
                opts.delim = parse_string(&d);
            }
            "-D" => {
                let d = it.next().unwrap_or_default();
                let chars: Vec<char> = d.chars().collect();
                if chars.len() != 1 {
                    return Err(format!(
                        "invalid lookup delim '{d}' (must be a single char)"
                    ));
                }
                opts.lookup_delim = chars[0];
            }
            "-e" => opts.exe_snippets.push(it.next().unwrap_or_default()),
            "-c" => opts.cond_snippets.push(it.next().unwrap_or_default()),
            "-k" | "--keys" => opts.output_keys = true,
            "-g" | "--group" => opts.group = true,
            "--merge" => opts.merge = Some(Merge::Shallow),
            "--deep-merge" => opts.merge = Some(Merge::Deep),
            "-f" => opts.input_files.push(it.next().unwrap_or_default()),
            "--validate" => opts.validate = true,
            other => {
                if !end_of_options && other.starts_with('-') && !other.is_empty() {
                    return Err(format!("unknown option '{other}'"));
                }
                opts.args.push(arg);
            }
        }
    }

    if opts.group && opts.merge.is_some() {
        return Err("cannot use -g|--group and --merge options together".into());
    }
    if opts.output_keys && !opts.args.is_empty() {
        return Err("cannot use -k|--keys option and lookup arguments together".into());
    }
    Ok(opts)
}

// Handle backslash escapes in -d argument, as json.js _parseString does.
fn parse_string(s: &str) -> String {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn read_input(opts: &Opts) -> io::Result<String> {
    if opts.input_files.is_empty() {
        let mut buf = String::new();
        io::stdin().read_to_string(&mut buf)?;
        Ok(buf)
    } else {
        let mut buf = String::new();
        for f in &opts.input_files {
            buf.push_str(&std::fs::read_to_string(f)?);
        }
        Ok(buf)
    }
}

fn run(opts: &Opts) -> anyhow::Result<ExitCode> {
    let raw = read_input(opts)?;

    let (header, body) = parse_input::strip_http_header(&raw, opts.drop_headers);
    let mut stdout = io::stdout();
    if !header.is_empty() {
        stdout.write_all(header.as_bytes())?;
    }

    if body.is_empty() {
        return Ok(ExitCode::SUCCESS);
    }

    process_chunk(&body, opts, &mut stdout)
}

fn process_chunk(chunk: &str, opts: &Opts, out: &mut impl Write) -> anyhow::Result<ExitCode> {
    let parsed = if opts.group {
        parse_input::parse_group(chunk)
    } else if let Some(m) = &opts.merge {
        parse_input::parse_merge(chunk, m)
    } else {
        parse_input::parse_plain(chunk)
    };

    let mut data = match parsed {
        Ok(v) => v,
        Err(e) => {
            if !opts.quiet {
                eprintln!("json: error: input is not JSON: {e}");
            }
            if !opts.validate {
                out.write_all(chunk.as_bytes())?;
                if !chunk.ends_with('\n') {
                    out.write_all(b"\n")?;
                }
            }
            return Ok(ExitCode::from(1));
        }
    };

    if opts.validate {
        return Ok(ExitCode::SUCCESS);
    }

    // -e: execute snippets (per-array-item unless -A or non-array).
    if !opts.exe_snippets.is_empty() {
        data = apply_exec(data, opts)?;
    }

    // -c: conditional filtering.
    if !opts.cond_snippets.is_empty() {
        data = apply_cond(data, opts)?;
    }

    // Lookups
    let lookups: Vec<Vec<String>> = opts
        .args
        .iter()
        .map(|l| lookup::parse_lookup(l, opts.lookup_delim))
        .collect();

    emit_output(data, &lookups, opts, out)
}

fn is_array_mode(opts: &Opts, data: &Value) -> bool {
    match opts.array {
        Some(true) => true,
        Some(false) => false,
        None => data.is_array(),
    }
}

fn apply_exec(mut data: Value, opts: &Opts) -> anyhow::Result<Value> {
    if is_array_mode(opts, &data) {
        let mut arrayified = false;
        let mut items: Vec<Value> = match data {
            Value::Array(a) => a,
            other => {
                arrayified = true;
                vec![other]
            }
        };
        for item in items.iter_mut() {
            for snip in &opts.exe_snippets {
                *item = eval::exec(item, snip)?;
            }
        }
        data = if arrayified {
            items.into_iter().next().unwrap_or(Value::Null)
        } else {
            Value::Array(items)
        };
    } else {
        for snip in &opts.exe_snippets {
            data = eval::exec(&data, snip)?;
        }
    }
    Ok(data)
}

fn apply_cond(data: Value, opts: &Opts) -> anyhow::Result<Value> {
    if is_array_mode(opts, &data) {
        let arrayified = !data.is_array();
        let items: Vec<Value> = match data {
            Value::Array(a) => a,
            other => vec![other],
        };
        let mut filtered: Vec<Value> = Vec::new();
        for item in items {
            let mut keep = true;
            for snip in &opts.cond_snippets {
                if !eval::cond(&item, snip)? {
                    keep = false;
                    break;
                }
            }
            if keep {
                filtered.push(item);
            }
        }
        if arrayified {
            Ok(filtered.into_iter().next().unwrap_or(Value::Array(vec![])))
        } else {
            Ok(Value::Array(filtered))
        }
    } else {
        let mut keep = true;
        for snip in &opts.cond_snippets {
            if !eval::cond(&data, snip)? {
                keep = false;
                break;
            }
        }
        if keep { Ok(data) } else { Ok(Value::Null) }
    }
}

// Emit a separator-terminated rendered value; the bool mirrors json.js
// `alwaysPrintSep` (emit sep even when value renders empty/undefined).
fn print_datum(
    datum: Option<&Value>,
    opts: &Opts,
    sep: &str,
    always_sep: bool,
    out: &mut impl Write,
) -> io::Result<()> {
    let rendered = output::render(datum, opts.output_mode, opts.json_indent);
    match rendered {
        Some(s) if !s.is_empty() => {
            out.write_all(s.as_bytes())?;
            out.write_all(sep.as_bytes())?;
        }
        _ => {
            if always_sep {
                out.write_all(sep.as_bytes())?;
            }
        }
    }
    Ok(())
}

fn emit_output(
    data: Value,
    lookups: &[Vec<String>],
    opts: &Opts,
    out: &mut impl Write,
) -> anyhow::Result<ExitCode> {
    // Note whether all lookups are pure array indeces (affects -j output).
    let mut lookups_are_indeces = false;

    // Build the row map / table when there are lookups.
    // We model json.js rows as ordered maps keyed by lookup.join('.').
    let array_mode = matches!(opts.array, Some(true));

    let mut table: Vec<Vec<(String, Value)>> = Vec::new();
    let mut row: Vec<(String, Value)> = Vec::new();

    if !lookups.is_empty() {
        if array_mode {
            let items: Vec<Value> = match &data {
                Value::Array(a) => a.clone(),
                other => vec![other.clone()],
            };
            for item in &items {
                let mut r: Vec<(String, Value)> = Vec::new();
                for lk in lookups {
                    if let Some(v) = lookup::lookup_datum(item, lk) {
                        r.push((lookup::join_lookup(lk), v));
                    }
                }
                table.push(r);
            }
        } else {
            if data.is_array() {
                lookups_are_indeces = lookups
                    .iter()
                    .all(|lk| lk.len() == 1 && lk[0].parse::<f64>().is_ok());
            }
            for lk in lookups {
                if let Some(v) = lookup::lookup_datum(&data, lk) {
                    row.push((lookup::join_lookup(lk), v));
                }
            }
        }
    }

    // --keys
    let keys_value: Option<Value> = if opts.output_keys {
        match &data {
            Value::Object(o) => Some(Value::Array(
                o.keys().map(|k| Value::String(k.clone())).collect(),
            )),
            Value::Array(a) => Some(Value::Array(
                (0..a.len())
                    .map(|i| Value::String(i.to_string()))
                    .collect(),
            )),
            _ => Some(Value::Array(vec![])),
        }
    } else {
        None
    };

    // OM_JSON output path. `None` means undefined (render nothing).
    if opts.output_mode == OutputMode::Json {
        let final_val: Option<Value> = if let Some(kv) = keys_value {
            Some(kv)
        } else if !lookups.is_empty() {
            if lookups.len() == 1 && !array_mode {
                // Single lookup: emit the raw value (may be undefined/missing).
                row.into_iter().next().map(|(_, v)| v)
            } else if lookups_are_indeces {
                let mut flat = Vec::new();
                for lk in lookups {
                    let key = lookup::join_lookup(lk);
                    if let Some((_, v)) = row.iter().find(|(k, _)| *k == key) {
                        flat.push(v.clone());
                    }
                }
                Some(Value::Array(flat))
            } else if array_mode {
                Some(Value::Array(
                    table
                        .into_iter()
                        .map(|r| Value::Object(r.into_iter().collect()))
                        .collect(),
                ))
            } else {
                Some(Value::Object(row.into_iter().collect()))
            }
        } else {
            Some(data)
        };
        print_datum(final_val.as_ref(), opts, "\n", false, out)?;
        return Ok(ExitCode::SUCCESS);
    }

    // Non-JSON output modes.
    if let Some(kv) = keys_value {
        print_datum(Some(&kv), opts, "\n", false, out)?;
        return Ok(ExitCode::SUCCESS);
    }

    if !lookups.is_empty() {
        if array_mode {
            for r in &table {
                let lookup_map: std::collections::HashMap<&str, &Value> =
                    r.iter().map(|(k, v)| (k.as_str(), v)).collect();
                for (i, lk) in lookups.iter().enumerate() {
                    let key = lookup::join_lookup(lk);
                    let v = lookup_map.get(key.as_str()).copied();
                    let sep = if i + 1 == lookups.len() { "\n" } else { &opts.delim };
                    print_datum(v, opts, sep, true, out)?;
                }
            }
        } else {
            let lookup_map: std::collections::HashMap<&str, &Value> =
                row.iter().map(|(k, v)| (k.as_str(), v)).collect();
            for lk in lookups {
                let key = lookup::join_lookup(lk);
                let v = lookup_map.get(key.as_str()).copied();
                print_datum(v, opts, "\n", false, out)?;
            }
        }
        return Ok(ExitCode::SUCCESS);
    }

    if array_mode {
        let items: Vec<Value> = match data {
            Value::Array(a) => a,
            other => vec![other],
        };
        for item in &items {
            print_datum(Some(item), opts, "\n", false, out)?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    print_datum(Some(&data), opts, "\n", false, out)?;
    Ok(ExitCode::SUCCESS)
}

fn print_help() {
    // Byte-exact copy of the node tool's help output.
    print!("{}", include_str!("help.txt"));
}
