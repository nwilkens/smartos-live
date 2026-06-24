// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Native Rust drop-in for the SmartOS `smartdc/lib/ntp_config` tool. Parses
//! the same option set (`a:f:s:to`) and emits byte-identical `/etc/inet/ntp.conf`
//! content, either to stdout or atomically to a file.

use std::io::Write;
use std::net::{Ipv4Addr, Ipv6Addr};

use anyhow::{Context, Result, bail};

struct Options {
    allow_list: Vec<String>,
    server_list: Vec<String>,
    trusted: bool,
    orphan: bool,
    filename: Option<String>,
}

/// POSIX getopt over an argument list, matching node-getopt's BasicParser for
/// the `a:f:s:to` optstring. `a`, `f`, `s` take an argument; `t`, `o` are flags.
struct GetOpt {
    args: Vec<String>,
    idx: usize,
    subidx: usize,
}

struct ParsedOpt {
    option: char,
    optarg: Option<String>,
}

impl GetOpt {
    fn new(args: Vec<String>) -> Self {
        // argv[0] is the program name; getopt begins scanning at argv[1].
        GetOpt { args, idx: 1, subidx: 0 }
    }

    fn takes_arg(c: char) -> Option<bool> {
        match c {
            'a' | 'f' | 's' => Some(true),
            't' | 'o' => Some(false),
            _ => None,
        }
    }

    fn next(&mut self) -> Result<Option<ParsedOpt>> {
        if self.idx >= self.args.len() {
            return Ok(None);
        }
        let arg = self.args[self.idx].clone();
        let bytes: Vec<char> = arg.chars().collect();

        if self.subidx == 0 {
            // Not in the middle of a clustered option group.
            if bytes.len() < 2 || bytes[0] != '-' {
                // First non-option argument: BasicParser stops here.
                return Ok(None);
            }
            if bytes.len() == 2 && bytes[1] == '-' {
                // "--" terminates option processing.
                self.idx += 1;
                return Ok(None);
            }
            self.subidx = 1;
        }

        let c = bytes[self.subidx];
        match GetOpt::takes_arg(c) {
            None => {
                // Unknown option: node-getopt surfaces '?'; the caller exits 1.
                self.advance_after_flag(&bytes);
                bail!("unknown option -{}", c);
            }
            Some(false) => {
                self.advance_after_flag(&bytes);
                Ok(Some(ParsedOpt { option: c, optarg: None }))
            }
            Some(true) => {
                let rest: String = bytes[self.subidx + 1..].iter().collect();
                if !rest.is_empty() {
                    // Argument is attached, e.g. -sFOO.
                    self.idx += 1;
                    self.subidx = 0;
                    Ok(Some(ParsedOpt { option: c, optarg: Some(rest) }))
                } else {
                    // Argument is the next argv element.
                    self.idx += 1;
                    self.subidx = 0;
                    if self.idx >= self.args.len() {
                        bail!("option -{} requires an argument", c);
                    }
                    let optarg = self.args[self.idx].clone();
                    self.idx += 1;
                    Ok(Some(ParsedOpt { option: c, optarg: Some(optarg) }))
                }
            }
        }
    }

    fn advance_after_flag(&mut self, bytes: &[char]) {
        if self.subidx + 1 < bytes.len() {
            self.subidx += 1;
        } else {
            self.idx += 1;
            self.subidx = 0;
        }
    }
}

fn parse_options() -> Result<Options> {
    let argv: Vec<String> = std::env::args().collect();
    let mut parser = GetOpt::new(argv);

    let mut out = Options {
        allow_list: Vec::new(),
        server_list: Vec::new(),
        trusted: false,
        orphan: false,
        filename: None,
    };

    loop {
        match parser.next() {
            Ok(None) => break,
            Ok(Some(opt)) => match opt.option {
                'a' => {
                    if let Some(a) = opt.optarg {
                        out.allow_list.push(a);
                    }
                }
                'f' => out.filename = opt.optarg,
                's' => {
                    if let Some(s) = opt.optarg {
                        add_server(&mut out.server_list, &s);
                    }
                }
                't' => out.trusted = true,
                'o' => out.orphan = true,
                _ => std::process::exit(1),
            },
            Err(_) => std::process::exit(1),
        }
    }

    if out.server_list.is_empty() {
        eprintln!("ERROR: must specify at least one server (-s)");
        std::process::exit(1);
    }

    Ok(out)
}

/// Split a -s argument on commas/whitespace, trim, and dedupe into server_list.
fn add_server(server_list: &mut Vec<String>, s: &str) {
    for part in s.split(|c: char| c == ',' || c.is_whitespace()) {
        let ss = part.trim();
        if ss.is_empty() {
            continue;
        }
        if !server_list.iter().any(|e| e == ss) {
            server_list.push(ss.to_string());
        }
    }
}

fn hexdigit(num: u32) -> String {
    let s = format!("{:x}", num);
    if s.len() == 1 { format!("0{}", s) } else { s }
}

/// True for the same dotted-quad forms node's net.isIPv4 accepts.
fn is_ipv4(x: &str) -> bool {
    x.parse::<Ipv4Addr>().is_ok()
}

fn is_ipv6(x: &str) -> bool {
    x.parse::<Ipv6Addr>().is_ok()
}

/// Mirror net.isIP: 4, 6, or 0.
fn is_ip(x: &str) -> u8 {
    if is_ipv4(x) {
        4
    } else if is_ipv6(x) {
        6
    } else {
        0
    }
}

/// Return the address-format subnet mask for `x` given address family `type`.
/// `x` may itself be an address-format mask (returned verbatim) or a CIDR
/// prefix length. Returns None for invalid input.
fn to_netmask(x: &str, ty: i32) -> Option<String> {
    // Parse as a number for the CIDR-suffix case. node uses Number(x): an empty
    // or whitespace-only string becomes 0, otherwise NaN-or-negative is rejected.
    let n: Option<i64> = parse_number(x).filter(|&v| v >= 0);

    match ty {
        4 => {
            if is_ipv4(x) {
                return Some(x.to_string());
            }
            match n {
                Some(v) if v <= 32 => {}
                _ => return None,
            }
        }
        6 => {
            if is_ipv6(x) {
                return Some(x.to_string());
            }
            match n {
                Some(v) if v <= 128 => {}
                _ => return None,
            }
        }
        _ => return None,
    }

    let n = n?;
    let mut out = String::new();
    let mut grp = 0;
    let nbytes = if ty == 4 { 4 } else { 16 };
    for byt in 0..nbytes {
        let mut mask: u32 = 0;
        for bit in 0..8 {
            if (byt * 8 + bit) < n {
                mask |= 0x80u32 >> bit;
            }
        }
        match ty {
            4 => {
                out.push_str(&mask.to_string());
                if byt < 3 {
                    out.push('.');
                }
            }
            6 => {
                out.push_str(&hexdigit(mask));
                grp += 1;
                if grp % 2 == 0 && byt < 15 {
                    out.push(':');
                }
            }
            _ => return None,
        }
    }
    Some(out)
}

/// Replicate JS Number(x) for the inputs to_netmask sees: leading/trailing
/// whitespace allowed, empty/whitespace -> 0, else parse as a number. Only the
/// integer subset matters here (CIDR prefix lengths).
fn parse_number(x: &str) -> Option<i64> {
    let t = x.trim();
    if t.is_empty() {
        return Some(0);
    }
    // Number() accepts decimals; a fractional prefix length is meaningless and
    // would never produce a valid mask, so reject non-integers.
    t.parse::<i64>().ok()
}

fn generate_file(
    servers: &[String],
    allowed_subnets: &[String],
    trusted: bool,
    orphan: bool,
) -> Result<String> {
    let mut out: Vec<String> = vec![
        "driftfile /var/ntp/ntp.drift".into(),
        "logfile /var/log/ntp.log".into(),
        "".into(),
        "# Ignore all network traffic by default".into(),
        "restrict default ignore".into(),
        "restrict -6 default ignore".into(),
        "".into(),
        "# Allow localhost to manage ntpd".into(),
        "restrict 127.0.0.1".into(),
        "restrict -6 ::1".into(),
        "".into(),
        "# Allow servers to reply to our queries".into(),
        "restrict source nomodify noquery notrap".into(),
    ];

    if trusted {
        out.push("".into());
        out.push("# Accept large time differences from servers".into());
        out.push("tinker panic 0".into());
    }

    if orphan {
        out.push("".into());
        out.push("# Operate in Oprhan mode when we cannot reach".into());
        out.push("# any upstream time servers for 90 seconds".into());
        out.push("tos orphan 10 orphanwait 90".into());
    }

    if !allowed_subnets.is_empty() {
        out.push("".into());
        out.push("# Allow local subnets to query this server".into());
        let mut seen: Vec<String> = Vec::new();
        for entry in allowed_subnets {
            let parts: Vec<&str> = entry.split('/').collect();
            if parts.len() != 2 {
                eprintln!("invalid local subnet: {}", entry);
                std::process::exit(1);
            }
            let fam: i32 = if is_ipv4(parts[0]) {
                4
            } else if is_ipv6(parts[0]) {
                6
            } else {
                -1
            };
            let mask = to_netmask(parts[1], fam);
            let mask = match mask {
                Some(m) if fam != -1 => m,
                _ => {
                    eprintln!("invalid local subnet: {}", entry);
                    std::process::exit(1);
                }
            };

            let key = format!("{}/{}", parts[0], mask);
            if !seen.iter().any(|k| k == &key) {
                let directive = if fam == 4 { "restrict" } else { "restrict -6" };
                out.push(format!("{} {} mask {}", directive, parts[0], mask));
                seen.push(key);
            }
        }
    }

    if !servers.is_empty() {
        out.push("".into());
        out.push("# Time Servers".into());
        for s in servers {
            // IPs use the "server" directive; anything else is a "pool".
            let directive = if is_ip(s) != 0 { "server" } else { "pool" };
            out.push(format!("{} {} burst iburst minpoll 4", directive, s));
        }
    }

    Ok(out.join("\n") + "\n")
}

fn write_file(path: &str, data: &str) -> Result<()> {
    let p = std::path::Path::new(path);
    let dir = p.parent().filter(|d| !d.as_os_str().is_empty());
    let base = p
        .file_name()
        .and_then(|b| b.to_str())
        .context("invalid output path")?;
    let pid = std::process::id();
    let tmpname = format!(".{}.{}", base, pid);
    let tmpfile = match dir {
        Some(d) => d.join(&tmpname),
        None => std::path::PathBuf::from(&tmpname),
    };

    if let Err(e) = write_tmp(&tmpfile, data) {
        eprintln!(
            "ERROR: writing file \"{}\": {}",
            tmpfile.display(),
            e
        );
        std::process::exit(1);
    }

    if let Err(e) = std::fs::rename(&tmpfile, p) {
        eprintln!(
            "ERROR: renaming file \"{}\" to \"{}\": {}",
            tmpfile.display(),
            path,
            e
        );
        let _ = std::fs::remove_file(&tmpfile);
        std::process::exit(1);
    }
    Ok(())
}

fn write_tmp(tmpfile: &std::path::Path, data: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o644)
        .open(tmpfile)?;
    f.write_all(data.as_bytes())?;
    Ok(())
}

fn main() -> Result<()> {
    let options = parse_options()?;
    let data = generate_file(
        &options.server_list,
        &options.allow_list,
        options.trusted,
        options.orphan,
    )?;

    match options.filename {
        Some(path) => write_file(&path, &data)?,
        None => {
            std::io::stdout().write_all(data.as_bytes())?;
        }
    }
    Ok(())
}
