// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Minimal evaluator for the common `-c CONDITION` forms documented in
//! node-bunyan: `this.FIELD OP VALUE`, `this.FIELD.indexOf("s") OP N`, and
//! the bare level constants (TRACE..FATAL). Not a full JS engine; conditions
//! outside these shapes return an error so the caller can warn and exit like
//! node would on a crash.

use anyhow::{Result, bail};
use serde_json::{Map, Value};

use crate::level;

#[derive(Clone, Copy, PartialEq)]
enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

pub struct Condition {
    left: Operand,
    cmp: Cmp,
    right: Operand,
}

#[derive(Clone)]
enum Operand {
    /// `this.field` path lookup.
    Field(Vec<String>),
    /// `this.field.indexOf("needle")`.
    IndexOf(Vec<String>, String),
    Num(f64),
    Str(String),
    Bool(bool),
    LevelConst(i64),
}

impl Condition {
    pub fn parse(src: &str) -> Result<Self> {
        let s = strip_return(src.trim());
        let (lhs, cmp, rhs) = split_cmp(&s)?;
        Ok(Condition {
            left: parse_operand(lhs.trim())?,
            cmp,
            right: parse_operand(rhs.trim())?,
        })
    }

    pub fn eval(&self, rec: &Map<String, Value>) -> bool {
        let l = resolve(&self.left, rec);
        let r = resolve(&self.right, rec);
        compare(&l, self.cmp, &r)
    }
}

/// node auto-"return"s a bare expression; tolerate an explicit `return`.
fn strip_return(s: &str) -> String {
    let t = s.strip_prefix("return").map(str::trim).unwrap_or(s);
    t.trim_end_matches(';').trim().to_string()
}

fn split_cmp(s: &str) -> Result<(String, Cmp, String)> {
    // Order matters: match 3-char then 2-char operators first.
    for (op, cmp) in [
        ("===", Cmp::Eq),
        ("!==", Cmp::Ne),
        ("==", Cmp::Eq),
        ("!=", Cmp::Ne),
        ("<=", Cmp::Le),
        (">=", Cmp::Ge),
        ("<", Cmp::Lt),
        (">", Cmp::Gt),
    ] {
        if let Some(idx) = find_top_level(s, op) {
            let lhs = s[..idx].to_string();
            let rhs = s[idx + op.len()..].to_string();
            return Ok((lhs, cmp, rhs));
        }
    }
    bail!("unsupported condition (no comparison operator): {s}")
}

/// Find an operator not inside quotes/parentheses.
fn find_top_level(s: &str, op: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth = 0i32;
    let mut in_str: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match in_str {
            Some(q) => {
                if c == b'\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    in_str = None;
                }
            }
            None => match c {
                b'"' | b'\'' => in_str = Some(c),
                b'(' | b'[' => depth += 1,
                b')' | b']' => depth -= 1,
                _ => {
                    if depth == 0 && s[i..].starts_with(op) {
                        // Avoid matching '<' inside '<=' etc.: ensure not a
                        // longer operator char follows for single-char ops.
                        return Some(i);
                    }
                }
            },
        }
        i += 1;
    }
    None
}

fn parse_operand(s: &str) -> Result<Operand> {
    if let Some(rest) = s.strip_prefix("this.") {
        if let Some(open) = rest.find(".indexOf(") {
            let path = rest[..open].split('.').map(str::to_string).collect();
            let inner = &rest[open + ".indexOf(".len()..];
            let needle = parse_string_lit(inner.trim_end_matches(')').trim())?;
            return Ok(Operand::IndexOf(path, needle));
        }
        let path = rest.split('.').map(str::to_string).collect();
        return Ok(Operand::Field(path));
    }
    if let Some(lvl) = match s {
        "TRACE" => Some(level::TRACE),
        "DEBUG" => Some(level::DEBUG),
        "INFO" => Some(level::INFO),
        "WARN" => Some(level::WARN),
        "ERROR" => Some(level::ERROR),
        "FATAL" => Some(level::FATAL),
        _ => None,
    } {
        return Ok(Operand::LevelConst(lvl));
    }
    match s {
        "true" => return Ok(Operand::Bool(true)),
        "false" => return Ok(Operand::Bool(false)),
        _ => {}
    }
    if let Ok(n) = s.parse::<f64>() {
        return Ok(Operand::Num(n));
    }
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        return Ok(Operand::Str(parse_string_lit(s)?));
    }
    bail!("unsupported condition operand: {s}")
}

fn parse_string_lit(s: &str) -> Result<String> {
    let s = s.trim();
    if s.len() >= 2 && (s.starts_with('"') || s.starts_with('\'')) {
        return Ok(s[1..s.len() - 1].to_string());
    }
    bail!("expected string literal: {s}")
}

#[derive(Clone)]
enum Resolved {
    Num(f64),
    Str(String),
    Bool(bool),
    Null,
}

fn lookup<'a>(rec: &'a Map<String, Value>, path: &[String]) -> Option<&'a Value> {
    let mut cur = rec.get(path.first()?)?;
    for key in &path[1..] {
        cur = cur.as_object()?.get(key)?;
    }
    Some(cur)
}

fn resolve(op: &Operand, rec: &Map<String, Value>) -> Resolved {
    match op {
        Operand::Num(n) => Resolved::Num(*n),
        Operand::Str(s) => Resolved::Str(s.clone()),
        Operand::Bool(b) => Resolved::Bool(*b),
        Operand::LevelConst(l) => Resolved::Num(*l as f64),
        Operand::Field(path) => match lookup(rec, path) {
            Some(Value::Number(n)) => Resolved::Num(n.as_f64().unwrap_or(f64::NAN)),
            Some(Value::String(s)) => Resolved::Str(s.clone()),
            Some(Value::Bool(b)) => Resolved::Bool(*b),
            _ => Resolved::Null,
        },
        Operand::IndexOf(path, needle) => match lookup(rec, path).and_then(Value::as_str) {
            Some(hay) => Resolved::Num(index_of(hay, needle)),
            None => Resolved::Num(-1.0),
        },
    }
}

fn index_of(hay: &str, needle: &str) -> f64 {
    match hay.find(needle) {
        Some(byte_idx) => hay[..byte_idx].chars().count() as f64,
        None => -1.0,
    }
}

fn compare(l: &Resolved, cmp: Cmp, r: &Resolved) -> bool {
    use Resolved::*;
    match (l, r) {
        (Num(a), Num(b)) => num_cmp(*a, cmp, *b),
        (Str(a), Str(b)) => str_cmp(a, cmp, b),
        (Bool(a), Bool(b)) => bool_cmp(*a, cmp, *b),
        // Mixed/null: only (in)equality is meaningful; mismatched types are
        // never equal (loose coercion edge cases are out of scope).
        (Null, Null) => matches!(cmp, Cmp::Eq | Cmp::Le | Cmp::Ge),
        _ => matches!(cmp, Cmp::Ne),
    }
}

fn num_cmp(a: f64, cmp: Cmp, b: f64) -> bool {
    match cmp {
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Gt => a > b,
        Cmp::Ge => a >= b,
    }
}

fn str_cmp(a: &str, cmp: Cmp, b: &str) -> bool {
    match cmp {
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        Cmp::Lt => a < b,
        Cmp::Le => a <= b,
        Cmp::Gt => a > b,
        Cmp::Ge => a >= b,
    }
}

fn bool_cmp(a: bool, cmp: Cmp, b: bool) -> bool {
    match cmp {
        Cmp::Eq => a == b,
        Cmp::Ne => a != b,
        _ => false,
    }
}
