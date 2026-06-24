// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! fwrule DSL parser. Faithful port of the jison-generated parser in
//! `src/fw/node_modules/fwrule/{parser.js,index.js,validators.js}`.
//!
//! The node parser is an LALR(1) table-driven parser; this is an equivalent
//! recursive-descent implementation over the same grammar. Token shapes,
//! validation order, and the language-version gating (`validateOKVersion`)
//! are reproduced so that error behavior matches.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::error::FwError;

/// Current (and default max) language version. Matches index.js CURR_VERSION.
pub const CURR_VERSION: u32 = 4;

/// A parsed protocol target: a single port, a port range, or "all".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortTarget {
    /// A single discrete port number.
    Port(u32),
    /// An inclusive port range.
    Range { start: u32, end: u32 },
    /// `PORT ALL`.
    All,
}

/// A parsed ICMP type target: "N", "N:C", or "all".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcmpTarget {
    /// Just an ICMP type.
    Type(u32),
    /// An ICMP type with a code.
    TypeCode(u32, u32),
    /// `TYPE ALL`.
    All,
}

/// A parsed protocol with its targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedProtocol {
    /// TCP with ports.
    Tcp(Vec<PortTarget>),
    /// UDP with ports.
    Udp(Vec<PortTarget>),
    /// ICMPv4 with types.
    Icmp(Vec<IcmpTarget>),
    /// ICMPv6 with types.
    Icmp6(Vec<IcmpTarget>),
    /// IPsec AH (targets implicitly `all`).
    Ah,
    /// IPsec ESP (targets implicitly `all`).
    Esp,
}

impl ParsedProtocol {
    /// The lowercase protocol name (matches the AST `protocol.name`).
    pub fn name(&self) -> &'static str {
        match self {
            ParsedProtocol::Tcp(_) => "tcp",
            ParsedProtocol::Udp(_) => "udp",
            ParsedProtocol::Icmp(_) => "icmp",
            ParsedProtocol::Icmp6(_) => "icmp6",
            ParsedProtocol::Ah => "ah",
            ParsedProtocol::Esp => "esp",
        }
    }
}

/// A target on one side of a rule (a `['type', value]` pair in the AST).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// `['ip', addr]`.
    Ip(String),
    /// `['subnet', 'addr/len']`.
    Subnet(String),
    /// `['tag', name]`.
    Tag(String),
    /// `['tag', [name, value]]`.
    TagValue(String, String),
    /// `['vm', uuid]`.
    Vm(String),
    /// `['wildcard', 'vmall'|'any']`.
    Wildcard(String),
}

/// The fully parsed rule AST, equivalent to the object returned by `parse()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRule {
    /// `from` side targets.
    pub from: Vec<Target>,
    /// `to` side targets.
    pub to: Vec<Target>,
    /// `allow` or `block`.
    pub action: String,
    /// The protocol and its targets.
    pub protocol: ParsedProtocol,
    /// Priority level (0 if not specified).
    pub priority: u32,
}

/// Options controlling parse behavior.
#[derive(Debug, Clone, Copy)]
pub struct ParseOpts {
    /// Maximum allowed language version.
    pub max_version: u32,
    /// Reject CIDRs with host bits set past the mask.
    pub enforce_subnet_mask: bool,
}

impl Default for ParseOpts {
    fn default() -> Self {
        ParseOpts { max_version: CURR_VERSION, enforce_subnet_mask: false }
    }
}

// --- Lexer ---

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    From,
    To,
    Ip,
    Subnet,
    Any,
    All,
    Tag,
    Vm,
    Vms,
    Or,
    And,
    Block,
    Allow,
    Port,
    Ports,
    Tcp,
    Udp,
    Icmp,
    Icmp6,
    Ah,
    Esp,
    Type,
    Code,
    Priority,
    Dash,
    Comma,
    Eq,
    LParen,
    RParen,
    /// Quoted string with the surrounding quotes stripped and body unescaped.
    Str(String),
    /// An IPv4 address literal (matched the IPV4ADDR regex).
    Ipv4(String),
    /// An IPv6 address literal (matched the IPV6ADDR regex).
    Ipv6(String),
    /// A CIDR suffix like `/24` (matched CIDRSUFFIX).
    CidrSuffix(String),
    /// A bare word (uuid, number, tag name, priolevel, ...).
    Word(String),
    Eof,
}

struct Lexer<'a> {
    input: &'a str,
    /// Byte offset into `input`.
    pos: usize,
}

/// The ordered keyword table: longest-match wins, each tested upper then lower.
/// Mirrors the jison-lex rule order. Each entry is (literal, token).
const KEYWORDS: &[(&str, Tok)] = &[
    // Note: PORTS before PORT, ICMP6 before ICMP, VMS before VM are handled
    // by longest-match selection below, not table order.
    ("FROM", Tok::From),
    ("TO", Tok::To),
    ("IP", Tok::Ip),
    ("SUBNET", Tok::Subnet),
    ("ANY", Tok::Any),
    ("ALL", Tok::All),
    ("TAG", Tok::Tag),
    ("VMS", Tok::Vms),
    ("VM", Tok::Vm),
    ("OR", Tok::Or),
    ("AND", Tok::And),
    ("BLOCK", Tok::Block),
    ("ALLOW", Tok::Allow),
    ("PORTS", Tok::Ports),
    ("PORT", Tok::Port),
    ("TCP", Tok::Tcp),
    ("UDP", Tok::Udp),
    ("ICMP6", Tok::Icmp6),
    ("ICMP", Tok::Icmp),
    ("AH", Tok::Ah),
    ("ESP", Tok::Esp),
    ("TYPE", Tok::Type),
    ("CODE", Tok::Code),
    ("PRIORITY", Tok::Priority),
];

impl<'a> Lexer<'a> {
    fn new(input: &'a str) -> Self {
        Lexer { input, pos: 0 }
    }

    fn rest(&self) -> &str {
        &self.input[self.pos..]
    }

    fn skip_ws(&mut self) {
        let r = self.rest();
        let trimmed = r.trim_start_matches(|c: char| c.is_whitespace());
        self.pos += r.len() - trimmed.len();
    }

    /// Word boundary check (jison `\b`): the char after a keyword must not be
    /// a word char `[A-Za-z0-9_]`.
    fn word_boundary_ok(rest: &str, kw_len: usize) -> bool {
        match rest[kw_len..].chars().next() {
            None => true,
            Some(c) => !(c.is_ascii_alphanumeric() || c == '_'),
        }
    }

    /// Produce the next token. Implements longest-match across all lexer
    /// rules: keywords, punctuation, STRING, IPV4ADDR, IPV6ADDR, CIDRSUFFIX,
    /// and WORD.
    fn next_token(&mut self) -> Result<Tok, FwError> {
        self.skip_ws();
        let rest = self.rest();
        if rest.is_empty() {
            return Ok(Tok::Eof);
        }

        // Candidate (length, token). Keep the longest. jison `next()` keeps the
        // first rule that produces the max-length match, so on ties earlier
        // rules in the rule list win; keywords precede STRING/IPV4/etc.
        let mut best: Option<(usize, Tok)> = None;

        // Keywords (case-insensitive, but matched as upper-or-lower literals
        // with a word boundary).
        for (lit, tok) in KEYWORDS {
            let len = lit.len();
            if rest.len() >= len {
                let head = &rest[..len];
                if (head.eq_ignore_ascii_case(lit))
                    && Self::word_boundary_ok(rest, len)
                {
                    Self::consider(&mut best, len, tok.clone());
                }
            }
        }

        // Single-char punctuation.
        let first = rest.as_bytes()[0];
        match first {
            b'-' => Self::consider(&mut best, 1, Tok::Dash),
            b',' => Self::consider(&mut best, 1, Tok::Comma),
            b'=' => Self::consider(&mut best, 1, Tok::Eq),
            b'(' => Self::consider(&mut best, 1, Tok::LParen),
            b')' => Self::consider(&mut best, 1, Tok::RParen),
            _ => {}
        }

        // STRING: /^"(?:\\["bfnrt/()\\]|\\u[a-fA-F0-9]{4}|[^"\\])*"/
        if first == b'"' {
            if let Some((len, body)) = Self::match_string(rest)? {
                let unescaped = tag_unescape(&body)?;
                Self::consider(&mut best, len, Tok::Str(unescaped));
            }
        }

        // IPV4ADDR: /^(([0-9]){1,3})\.(([0-9]){1,3})\.(([0-9]){1,3})\.(([0-9]){1,3})/
        if let Some(len) = Self::match_ipv4(rest) {
            Self::consider(&mut best, len, Tok::Ipv4(rest[..len].to_string()));
        }

        // IPV6ADDR: /^([0-9a-f])*:([0-9a-f])*:[:0-9a-f]*/
        if let Some(len) = Self::match_ipv6(rest) {
            Self::consider(&mut best, len, Tok::Ipv6(rest[..len].to_string()));
        }

        // CIDRSUFFIX: /^\/([0-9]){1,3}/
        if first == b'/' {
            if let Some(len) = Self::match_cidr(rest) {
                Self::consider(
                    &mut best,
                    len,
                    Tok::CidrSuffix(rest[..len].to_string()),
                );
            }
        }

        // WORD: /^[-a-zA-Z0-9_]+/  (the catch-all)
        if let Some(len) = Self::match_word(rest) {
            Self::consider(&mut best, len, Tok::Word(rest[..len].to_string()));
        }

        match best {
            Some((len, tok)) => {
                self.pos += len;
                Ok(tok)
            }
            None => Err(FwError::rule(format!(
                "Lexical error: Unrecognized text near \"{}\"",
                &rest[..rest.len().min(20)]
            ))),
        }
    }

    /// Keep the longest candidate; on a tie keep the existing (earlier) one,
    /// matching jison's "first rule with the max length wins" behavior, given
    /// our insertion order (keywords, punct, string, ipv4, ipv6, cidr, word).
    fn consider(best: &mut Option<(usize, Tok)>, len: usize, tok: Tok) {
        match best {
            Some((blen, _)) if *blen >= len => {}
            _ => *best = Some((len, tok)),
        }
    }

    fn match_string(rest: &str) -> Result<Option<(usize, String)>, FwError> {
        let bytes = rest.as_bytes();
        let mut i = 1; // skip opening quote
        let mut body = String::new();
        while i < bytes.len() {
            let c = bytes[i];
            if c == b'"' {
                return Ok(Some((i + 1, body)));
            }
            if c == b'\\' {
                if i + 1 >= bytes.len() {
                    return Ok(None);
                }
                let n = bytes[i + 1];
                match n {
                    b'"' | b'b' | b'f' | b'n' | b'r' | b't' | b'/' | b'('
                    | b')' | b'\\' => {
                        body.push('\\');
                        body.push(n as char);
                        i += 2;
                    }
                    b'u' => {
                        if i + 6 > bytes.len() {
                            return Ok(None);
                        }
                        let hex = &rest[i + 2..i + 6];
                        if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                            return Ok(None);
                        }
                        body.push_str(&rest[i..i + 6]);
                        i += 6;
                    }
                    _ => return Ok(None),
                }
            } else {
                // [^"\\] including multibyte UTF-8.
                let ch_len = utf8_char_len(c);
                body.push_str(&rest[i..i + ch_len]);
                i += ch_len;
            }
        }
        Ok(None)
    }

    fn match_ipv4(rest: &str) -> Option<usize> {
        // (\d{1,3}\.){3}\d{1,3}, greedy up to 3 digits per octet.
        let b = rest.as_bytes();
        let mut i = 0;
        for octet in 0..4 {
            let start = i;
            let mut digits = 0;
            while i < b.len() && b[i].is_ascii_digit() && digits < 3 {
                i += 1;
                digits += 1;
            }
            if i == start {
                return None;
            }
            if octet < 3 {
                if i >= b.len() || b[i] != b'.' {
                    return None;
                }
                i += 1;
            }
        }
        Some(i)
    }

    fn match_ipv6(rest: &str) -> Option<usize> {
        // /^([0-9a-f])*:([0-9a-f])*:[:0-9a-f]*/
        let b = rest.as_bytes();
        let is_lhex = |c: u8| c.is_ascii_digit() || (b'a'..=b'f').contains(&c);
        let mut i = 0;
        while i < b.len() && is_lhex(b[i]) {
            i += 1;
        }
        if i >= b.len() || b[i] != b':' {
            return None;
        }
        i += 1;
        while i < b.len() && is_lhex(b[i]) {
            i += 1;
        }
        if i >= b.len() || b[i] != b':' {
            return None;
        }
        i += 1;
        while i < b.len() && (is_lhex(b[i]) || b[i] == b':') {
            i += 1;
        }
        Some(i)
    }

    fn match_cidr(rest: &str) -> Option<usize> {
        let b = rest.as_bytes();
        if b[0] != b'/' {
            return None;
        }
        let mut i = 1;
        let mut digits = 0;
        while i < b.len() && b[i].is_ascii_digit() && digits < 3 {
            i += 1;
            digits += 1;
        }
        if digits == 0 { None } else { Some(i) }
    }

    fn match_word(rest: &str) -> Option<usize> {
        let b = rest.as_bytes();
        let is_word = |c: u8| {
            c == b'-' || c.is_ascii_alphanumeric() || c == b'_'
        };
        let mut i = 0;
        while i < b.len() && is_word(b[i]) {
            i += 1;
        }
        if i == 0 { None } else { Some(i) }
    }
}

fn utf8_char_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

// --- tag escape/unescape (rule.js) ---

/// Reverse of `tagEscape`. Throws on an unknown escape, matching node.
pub fn tag_unescape(ostr: &str) -> Result<String, FwError> {
    let bytes: Vec<char> = ostr.chars().collect();
    let mut out = String::new();
    let mut cur = 0;
    while cur < bytes.len() {
        let val = bytes[cur];
        if val == '\\' {
            let escaped = bytes.get(cur + 1).copied();
            match escaped {
                Some('u') => {
                    if cur + 6 > bytes.len() {
                        return Err(FwError::generic(
                            "Invalid escape sequence",
                        ));
                    }
                    let hex: String = bytes[cur + 2..cur + 6].iter().collect();
                    let code = u32::from_str_radix(&hex, 16).map_err(|_| {
                        FwError::generic("Invalid escape sequence")
                    })?;
                    match char::from_u32(code) {
                        Some(c) => out.push(c),
                        None => {
                            return Err(FwError::generic(
                                "Invalid escape sequence",
                            ));
                        }
                    }
                    cur += 5;
                }
                Some(c) => {
                    let mapped = match c {
                        '"' => '"',
                        'b' => '\u{0008}',
                        'f' => '\u{000C}',
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        '/' => '/',
                        '(' => '(',
                        ')' => ')',
                        '\\' => '\\',
                        _ => {
                            return Err(FwError::generic(format!(
                                "Invalid escape sequence \"\\{c}\"!"
                            )));
                        }
                    };
                    out.push(mapped);
                    cur += 1;
                }
                None => {
                    return Err(FwError::generic("Invalid escape sequence"));
                }
            }
        } else {
            out.push(val);
        }
        cur += 1;
    }
    Ok(out)
}

// --- Validators (validators.js + index.js yy.*) ---

fn validate_ipv4_address(ip: &str) -> Result<(), FwError> {
    // node net.isIPv4 requires canonical dotted-quad (no leading-zero / >255
    // octets). std parse accepts "1.2.3.4"; require the re-rendered form to
    // match, then reject the unusable all-zeros / broadcast addresses.
    let canonical = ip.parse::<Ipv4Addr>().is_ok_and(|a| a.to_string() == ip);
    if canonical && ip != "255.255.255.255" && ip != "0.0.0.0" {
        Ok(())
    } else {
        Err(FwError::rule(format!("IPv4 address \"{ip}\" is invalid")))
    }
}

fn validate_ipv6_address(ip: &str) -> Result<(), FwError> {
    if ip.parse::<Ipv6Addr>().is_ok() {
        Ok(())
    } else {
        Err(FwError::rule(format!("IPv6 address \"{ip}\" is invalid")))
    }
}

fn validate_subnet(input: &str, enforce: bool) -> Result<(), FwError> {
    let parts: Vec<&str> = input.splitn(2, '/').collect();
    if parts.len() != 2 {
        return Err(FwError::rule(format!(
            "Subnet \"{input}\" is invalid (bad address component)"
        )));
    }
    let addr_str = parts[0];
    let plen: u32 = parts[1].parse().map_err(|_| {
        FwError::rule(format!(
            "Subnet \"{input}\" is invalid (bad prefix length)"
        ))
    })?;

    if let Ok(v4) = addr_str.parse::<Ipv4Addr>() {
        if addr_str.parse::<Ipv4Addr>().map(|a| a.to_string()) != Ok(addr_str.to_string()) {
            return Err(FwError::rule(format!(
                "Subnet \"{input}\" is invalid (bad address component)"
            )));
        }
        if plen > 32 {
            return Err(FwError::rule(format!(
                "Subnet \"{input}\" is invalid (bad prefix length)"
            )));
        }
        let mask: u32 = if plen == 0 { 0 } else { u32::MAX << (32 - plen) };
        if enforce && u32::from(v4) & !mask != 0 {
            return Err(FwError::rule(format!(
                "Subnet \"{input}\" is invalid (bits set to right of mask)"
            )));
        }
        Ok(())
    } else if let Ok(v6) = addr_str.parse::<Ipv6Addr>() {
        if plen > 128 {
            return Err(FwError::rule(format!(
                "Subnet \"{input}\" is invalid (bad prefix length)"
            )));
        }
        let mask: u128 =
            if plen == 0 { 0 } else { u128::MAX << (128 - plen) };
        if enforce && u128::from(v6) & !mask != 0 {
            return Err(FwError::rule(format!(
                "Subnet \"{input}\" is invalid (bits set to right of mask)"
            )));
        }
        Ok(())
    } else {
        Err(FwError::rule(format!(
            "Subnet \"{input}\" is invalid (bad address component)"
        )))
    }
}

fn validate_uuid(text: &str) -> Result<(), FwError> {
    if crate::model::is_uuid(text) {
        Ok(())
    } else {
        Err(FwError::rule(format!("UUID \"{text}\" is invalid")))
    }
}

fn validate_priority_level(num: &str) -> Result<u32, FwError> {
    // node: throw if NON_DIGIT_RE.test(num) || Number(num) < 0 || > 100.
    if num.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = num.parse::<i64>()
            && (0..=100).contains(&n)
        {
            return Ok(n as u32);
        }
    }
    Err(FwError::rule(format!("Priority level \"{num}\" is invalid")))
}

fn validate_port_number(num: &str) -> Result<u32, FwError> {
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = num.parse::<i64>()
            && (1..=65535).contains(&n)
        {
            return Ok(n as u32);
        }
    }
    Err(FwError::rule(format!("Port number \"{num}\" is invalid")))
}

fn validate_icmp(num: &str, kind: &str) -> Result<u32, FwError> {
    if !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit()) {
        if let Ok(n) = num.parse::<i64>()
            && (0..=255).contains(&n)
        {
            return Ok(n as u32);
        }
    }
    Err(FwError::rule(format!("ICMP {kind} \"{num}\" is invalid")))
}

fn validate_ok_version(
    ver: u32,
    feature: &str,
    opts: &ParseOpts,
) -> Result<(), FwError> {
    if ver > opts.max_version {
        Err(FwError::rule(format!(
            "The rule uses a feature ({feature}) newer than this API allows"
        )))
    } else {
        Ok(())
    }
}

// --- Recursive-descent parser ---

struct Parser<'a> {
    lexer: Lexer<'a>,
    cur: Tok,
    opts: ParseOpts,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str, opts: ParseOpts) -> Result<Self, FwError> {
        let mut lexer = Lexer::new(input);
        let cur = lexer.next_token()?;
        Ok(Parser { lexer, cur, opts })
    }

    fn bump(&mut self) -> Result<Tok, FwError> {
        let next = self.lexer.next_token()?;
        Ok(std::mem::replace(&mut self.cur, next))
    }

    fn expect(&mut self, want: &Tok, label: &str) -> Result<(), FwError> {
        if &self.cur == want {
            self.bump()?;
            Ok(())
        } else {
            Err(FwError::rule(format!(
                "Parse error: expected {label}, found {:?}",
                self.cur
            )))
        }
    }

    fn parse_rule(&mut self) -> Result<ParsedRule, FwError> {
        self.expect(&Tok::From, "'FROM'")?;
        let from = self.parse_target_list()?;
        self.expect(&Tok::To, "'TO'")?;
        let to = self.parse_target_list()?;
        let action = self.parse_action()?;
        let protocol = self.parse_protocol()?;

        let mut priority = 0;
        if self.cur == Tok::Priority {
            validate_ok_version(4, "priority levels", &self.opts)?;
            self.bump()?;
            priority = self.parse_priolevel()?;
        }

        if self.cur != Tok::Eof {
            return Err(FwError::rule(format!(
                "Parse error: expected end of input, found {:?}",
                self.cur
            )));
        }

        Ok(ParsedRule { from, to, action, protocol, priority })
    }

    fn parse_priolevel(&mut self) -> Result<u32, FwError> {
        match self.bump()? {
            Tok::Word(w) => validate_priority_level(&w),
            other => Err(FwError::rule(format!(
                "Parse error: expected priority level, found {other:?}"
            ))),
        }
    }

    fn parse_target_list(&mut self) -> Result<Vec<Target>, FwError> {
        match &self.cur {
            Tok::Any => {
                self.bump()?;
                Ok(vec![Target::Wildcard("any".to_string())])
            }
            Tok::All => {
                self.bump()?;
                self.expect(&Tok::Vms, "'VMS'")?;
                Ok(vec![Target::Wildcard("vmall".to_string())])
            }
            Tok::LParen => {
                self.bump()?;
                let mut list = self.parse_target()?;
                while self.cur == Tok::Or {
                    self.bump()?;
                    list.extend(self.parse_target()?);
                }
                self.expect(&Tok::RParen, "')'")?;
                Ok(list)
            }
            _ => self.parse_target(),
        }
    }

    fn parse_target(&mut self) -> Result<Vec<Target>, FwError> {
        match self.cur.clone() {
            Tok::Ip => {
                self.bump()?;
                match self.bump()? {
                    Tok::Ipv4(a) => {
                        validate_ipv4_address(&a)?;
                        Ok(vec![Target::Ip(a)])
                    }
                    Tok::Ipv6(a) => {
                        validate_ok_version(3, "IPv6", &self.opts)?;
                        validate_ipv6_address(&a)?;
                        Ok(vec![Target::Ip(a)])
                    }
                    other => Err(FwError::rule(format!(
                        "Parse error: expected ip address, found {other:?}"
                    ))),
                }
            }
            Tok::Subnet => {
                self.bump()?;
                let addr = self.bump()?;
                let suffix = match self.bump()? {
                    Tok::CidrSuffix(s) => s,
                    other => {
                        return Err(FwError::rule(format!(
                            "Parse error: expected CIDRSUFFIX, found {other:?}"
                        )));
                    }
                };
                match addr {
                    Tok::Ipv4(a) => {
                        let cidr = format!("{a}{suffix}");
                        validate_subnet(&cidr, self.opts.enforce_subnet_mask)?;
                        Ok(vec![Target::Subnet(cidr)])
                    }
                    Tok::Ipv6(a) => {
                        validate_ok_version(3, "IPv6", &self.opts)?;
                        let cidr = format!("{a}{suffix}");
                        validate_subnet(&cidr, self.opts.enforce_subnet_mask)?;
                        Ok(vec![Target::Subnet(cidr)])
                    }
                    other => Err(FwError::rule(format!(
                        "Parse error: expected subnet address, found {other:?}"
                    ))),
                }
            }
            Tok::Vm => {
                self.bump()?;
                match self.bump()? {
                    Tok::Word(w) => {
                        validate_uuid(&w)?;
                        Ok(vec![Target::Vm(w)])
                    }
                    other => Err(FwError::rule(format!(
                        "Parse error: expected uuid, found {other:?}"
                    ))),
                }
            }
            Tok::Tag => {
                self.bump()?;
                let name = self.parse_tag_string()?;
                if self.cur == Tok::Eq {
                    self.bump()?;
                    let val = self.parse_tag_string()?;
                    Ok(vec![Target::TagValue(name, val)])
                } else {
                    Ok(vec![Target::Tag(name)])
                }
            }
            other => Err(FwError::rule(format!(
                "Parse error: expected target, found {other:?}"
            ))),
        }
    }

    fn parse_tag_string(&mut self) -> Result<String, FwError> {
        match self.bump()? {
            Tok::Word(w) => Ok(w),
            Tok::Str(s) => Ok(s),
            other => Err(FwError::rule(format!(
                "Parse error: expected tag string, found {other:?}"
            ))),
        }
    }

    fn parse_action(&mut self) -> Result<String, FwError> {
        match self.bump()? {
            Tok::Block => Ok("block".to_string()),
            Tok::Allow => Ok("allow".to_string()),
            other => Err(FwError::rule(format!(
                "Parse error: expected BLOCK or ALLOW, found {other:?}"
            ))),
        }
    }

    fn parse_protocol(&mut self) -> Result<ParsedProtocol, FwError> {
        match self.bump()? {
            Tok::Tcp => Ok(ParsedProtocol::Tcp(self.parse_port_list()?)),
            Tok::Udp => Ok(ParsedProtocol::Udp(self.parse_port_list()?)),
            Tok::Icmp => Ok(ParsedProtocol::Icmp(self.parse_type_list()?)),
            Tok::Icmp6 => {
                validate_ok_version(3, "IPv6", &self.opts)?;
                Ok(ParsedProtocol::Icmp6(self.parse_type_list()?))
            }
            Tok::Ah => {
                validate_ok_version(4, "AH", &self.opts)?;
                Ok(ParsedProtocol::Ah)
            }
            Tok::Esp => {
                validate_ok_version(4, "ESP", &self.opts)?;
                Ok(ParsedProtocol::Esp)
            }
            other => Err(FwError::rule(format!(
                "Parse error: expected protocol, found {other:?}"
            ))),
        }
    }

    fn parse_port_list(&mut self) -> Result<Vec<PortTarget>, FwError> {
        match &self.cur {
            Tok::LParen => {
                self.bump()?;
                let mut list = vec![self.parse_port()?];
                while self.cur == Tok::And {
                    self.bump()?;
                    list.push(self.parse_port()?);
                }
                self.expect(&Tok::RParen, "')'")?;
                Ok(list)
            }
            Tok::Ports => {
                validate_ok_version(2, "port ranges", &self.opts)?;
                self.bump()?;
                let mut list = vec![self.parse_portrange()?];
                while self.cur == Tok::Comma {
                    self.bump()?;
                    list.push(self.parse_portrange()?);
                }
                Ok(list)
            }
            Tok::Port => {
                // Either `PORT ALL` or a single `PORT N`.
                self.bump()?;
                if self.cur == Tok::All {
                    self.bump()?;
                    Ok(vec![PortTarget::All])
                } else {
                    let n = self.parse_portnumber()?;
                    Ok(vec![PortTarget::Port(n)])
                }
            }
            other => Err(FwError::rule(format!(
                "Parse error: expected port list, found {other:?}"
            ))),
        }
    }

    fn parse_port(&mut self) -> Result<PortTarget, FwError> {
        self.expect(&Tok::Port, "'PORT'")?;
        Ok(PortTarget::Port(self.parse_portnumber()?))
    }

    fn parse_portnumber(&mut self) -> Result<u32, FwError> {
        match self.bump()? {
            Tok::Word(w) => validate_port_number(&w),
            other => Err(FwError::rule(format!(
                "Parse error: expected port number, found {other:?}"
            ))),
        }
    }

    /// portrange: WORD ("createMaybePortRange": handles "N" or "N-M" lexed as
    /// WORD), or WORD '-' WORD.
    fn parse_portrange(&mut self) -> Result<PortTarget, FwError> {
        let first = match self.bump()? {
            Tok::Word(w) => w,
            other => {
                return Err(FwError::rule(format!(
                    "Parse error: expected port number, found {other:?}"
                )));
            }
        };
        if self.cur == Tok::Dash {
            // WORD '-' WORD form.
            self.bump()?;
            let second = match self.bump()? {
                Tok::Word(w) => w,
                other => {
                    return Err(FwError::rule(format!(
                        "Parse error: expected port number, found {other:?}"
                    )));
                }
            };
            let s = validate_port_number(&first)?;
            let e = validate_port_number(&second)?;
            if e < s {
                return Err(FwError::rule(format!(
                    "The end of the range ({e}) cannot be less than the start ({s})"
                )));
            }
            Ok(PortTarget::Range { start: s, end: e })
        } else {
            // createMaybePortRange: WORD might contain a dash (e.g. lexed
            // "1024-2048" as one WORD? No: '-' is its own token, but WORD
            // regex includes '-', so "1024-2048" lexes as a single WORD.
            // Handle that here.
            self.maybe_port_range(&first)
        }
    }

    fn maybe_port_range(&self, num: &str) -> Result<PortTarget, FwError> {
        let parts: Vec<&str> = num.split('-').collect();
        match parts.len() {
            1 => Ok(PortTarget::Port(validate_port_number(parts[0])?)),
            2 => {
                let s = validate_port_number(parts[0])?;
                let e = validate_port_number(parts[1])?;
                if e < s {
                    return Err(FwError::rule(format!(
                        "The end of the range ({e}) cannot be less than the start ({s})"
                    )));
                }
                Ok(PortTarget::Range { start: s, end: e })
            }
            _ => Err(FwError::rule(format!(
                "\"{num}\" is not a valid port number or range"
            ))),
        }
    }

    fn parse_type_list(&mut self) -> Result<Vec<IcmpTarget>, FwError> {
        match &self.cur {
            Tok::LParen => {
                self.bump()?;
                let mut list = vec![self.parse_type()?];
                while self.cur == Tok::And {
                    self.bump()?;
                    list.push(self.parse_type()?);
                }
                self.expect(&Tok::RParen, "')'")?;
                Ok(list)
            }
            _ => Ok(vec![self.parse_type()?]),
        }
    }

    fn parse_type(&mut self) -> Result<IcmpTarget, FwError> {
        self.expect(&Tok::Type, "'TYPE'")?;
        if self.cur == Tok::All {
            validate_ok_version(3, "all ICMP types", &self.opts)?;
            self.bump()?;
            return Ok(IcmpTarget::All);
        }
        let ty = match self.bump()? {
            Tok::Word(w) => validate_icmp(&w, "type")?,
            other => {
                return Err(FwError::rule(format!(
                    "Parse error: expected icmp type, found {other:?}"
                )));
            }
        };
        if self.cur == Tok::Code {
            self.bump()?;
            let code = match self.bump()? {
                Tok::Word(w) => validate_icmp(&w, "code")?,
                other => {
                    return Err(FwError::rule(format!(
                        "Parse error: expected icmp code, found {other:?}"
                    )));
                }
            };
            Ok(IcmpTarget::TypeCode(ty, code))
        } else {
            Ok(IcmpTarget::Type(ty))
        }
    }
}

/// Parse a rule string into a [`ParsedRule`], mirroring node `parse()`.
pub fn parse(input: &str, opts: ParseOpts) -> Result<ParsedRule, FwError> {
    let mut p = Parser::new(input, opts)?;
    p.parse_rule()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> ParsedRule {
        parse(s, ParseOpts::default()).expect("parse")
    }

    #[test]
    fn icmp6_type_all() {
        let r = p("FROM any TO all vms ALLOW icmp6 TYPE all");
        assert_eq!(r.action, "allow");
        assert_eq!(r.protocol, ParsedProtocol::Icmp6(vec![IcmpTarget::All]));
        assert_eq!(r.from, vec![Target::Wildcard("any".into())]);
        assert_eq!(r.to, vec![Target::Wildcard("vmall".into())]);
    }

    #[test]
    fn tcp_port() {
        let r = p("FROM any TO all vms ALLOW tcp PORT 22");
        assert_eq!(r.protocol, ParsedProtocol::Tcp(vec![PortTarget::Port(22)]));
    }

    #[test]
    fn ports_range() {
        let r = p("FROM any TO all vms ALLOW udp PORTS 67, 68, 1024-2048");
        match r.protocol {
            ParsedProtocol::Udp(v) => {
                assert_eq!(v[0], PortTarget::Port(67));
                assert_eq!(v[1], PortTarget::Port(68));
                assert_eq!(v[2], PortTarget::Range { start: 1024, end: 2048 });
            }
            _ => panic!("wrong proto"),
        }
    }

    #[test]
    fn tag_with_value_quoted() {
        let r = p("FROM tag \"foo\" = \"bar\" TO all vms ALLOW tcp PORT 80");
        assert_eq!(
            r.from,
            vec![Target::TagValue("foo".into(), "bar".into())]
        );
    }

    #[test]
    fn priority_v4() {
        let r = p("FROM any TO all vms BLOCK tcp PORT 25 PRIORITY 50");
        assert_eq!(r.priority, 50);
    }

    #[test]
    fn ipv6_target() {
        let r = p("FROM ip fe80::1 TO all vms ALLOW tcp PORT 22");
        assert_eq!(r.from, vec![Target::Ip("fe80::1".into())]);
    }

    #[test]
    fn version_gate_priority() {
        let opts = ParseOpts { max_version: 3, ..Default::default() };
        let e = parse(
            "FROM any TO all vms BLOCK tcp PORT 25 PRIORITY 50",
            opts,
        );
        assert!(e.is_err());
    }

    #[test]
    fn icmp_type_code() {
        let r = p("FROM any TO all vms ALLOW icmp (TYPE 8 AND TYPE 0)");
        assert_eq!(
            r.protocol,
            ParsedProtocol::Icmp(vec![
                IcmpTarget::Type(8),
                IcmpTarget::Type(0)
            ])
        );
    }
}
