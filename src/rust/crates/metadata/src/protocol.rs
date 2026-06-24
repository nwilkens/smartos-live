// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The mdata line protocol: request parsing and response framing for V1
//! (plain text) and V2 (CRC32-framed, base64) on a single connection.
//!
//! The wire is line-oriented: a peer accumulates bytes, splits on `\n`, and
//! dispatches each complete line. A line beginning with `V2 ` is a framed V2
//! request; anything else is V1. This mirrors `agent.js` `_metadataHandler`,
//! including the lenient per-line classification (a connection may mix V1 and
//! V2 lines after `NEGOTIATE V2`).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

use crate::crc32::crc32_calc;

/// A parsed metadata command, already unboxed from any V2 framing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// `GET <key>` (V1 or V2). `is_v2` selects the response envelope.
    Get { key: String, is_v2: bool },
    /// `KEYS` (mdata-list).
    Keys { is_v2: bool },
    /// `NEGOTIATE <ver>` (V1 only). `ver` is the requested version string.
    Negotiate { ver: String },
    /// `PUT <key> <value>` (V2 only; both args base64-decoded already).
    Put { key: String, value: String },
    /// `DELETE <key>` (V2 only; key is the decoded plaintext).
    Delete { key: String },
    /// An unknown command name (`cmd`), to be answered with FAILURE.
    Unknown { cmd: String, is_v2: bool },
    /// A line the server answers with the literal `invalid command\n`
    /// (unparseable, or an empty-arg GET/NEGOTIATE in V1).
    Invalid,
    /// A malformed V2 frame (bad regex/length/crc): the server writes NOTHING
    /// and silently drops it. The caller must not respond.
    SilentDrop,
}

/// The reqid carried by a V2 request, needed to build its response frame.
/// V1 requests have no reqid.
#[derive(Debug, Clone, Default)]
pub struct V2Ctx {
    pub reqid: String,
}

/// Parse one complete request line (without its trailing `\n`). Returns the
/// decoded [`Request`] and, for V2 frames, the [`V2Ctx`] needed to frame the
/// reply.
#[must_use]
pub fn parse_line(line: &str) -> (Request, V2Ctx) {
    // node: data.trimRight().replace(/\n$/, '').match(/^([^\s]+)\s?(.*)/)
    let trimmed = line.trim_end();
    let trimmed = trimmed.strip_suffix('\n').unwrap_or(trimmed);

    let Some((cmd, want)) = split_cmd_want(trimmed) else {
        return (Request::Invalid, V2Ctx::default());
    };

    // node: (cmd === 'NEGOTIATE' || cmd === 'GET') && !want -> invalid command
    if (cmd == "NEGOTIATE" || cmd == "GET") && want.is_empty() {
        return (Request::Invalid, V2Ctx::default());
    }

    if cmd == "V2" {
        return parse_v2(want);
    }

    let req = match cmd {
        "GET" => Request::Get {
            key: want.trim().to_string(),
            is_v2: false,
        },
        "KEYS" => Request::Keys { is_v2: false },
        "NEGOTIATE" => Request::Negotiate {
            ver: want.to_string(),
        },
        // PUT/DELETE are V2-only; in V1 they fall through to Unknown -> FAILURE.
        other => Request::Unknown {
            cmd: other.to_string(),
            is_v2: false,
        },
    };
    (req, V2Ctx::default())
}

/// node: data.match(/^([^\s]+)\s?(.*)/). cmd = first non-whitespace run;
/// want = the remainder after a single optional separating space.
fn split_cmd_want(s: &str) -> Option<(&str, &str)> {
    if s.is_empty() {
        return None;
    }
    // First token is run of non-whitespace; a leading whitespace fails the
    // anchored `^([^\s]+)` regex (node returns null -> invalid command).
    let first = s.chars().next()?;
    if first.is_whitespace() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(i) => {
            let cmd = &s[..i];
            // `\s?` consumes exactly one separating whitespace char.
            let rest_start = i + s[i..].chars().next().map_or(0, char::len_utf8);
            Some((cmd, &s[rest_start..]))
        }
        None => Some((s, "")),
    }
}

/// Parse the inner of a `V2 <clen> <crc> <reqid> <CMD> [<b64arg>]` frame
/// (everything after the leading `V2 `). Validates length and CRC; a failure
/// is a silent drop (no response), matching the node agent.
fn parse_v2(inframe: &str) -> (Request, V2Ctx) {
    // node regex: /\s*([0-9]+)\s+([0-9a-f]+)\s+([0-9a-f]+)\s+(.*)/
    let Some((clen_s, crc, reqid, body)) = split_v2_fields(inframe) else {
        return (Request::SilentDrop, V2Ctx::default());
    };

    // clen must be > 0 and equal the byte length of "<reqid> <body>".
    let Ok(clen) = clen_s.parse::<usize>() else {
        return (Request::SilentDrop, V2Ctx::default());
    };
    let checked = format!("{reqid} {body}");
    if clen == 0 || clen != checked.len() {
        return (Request::SilentDrop, V2Ctx::default());
    }

    if crc32_calc(checked.as_bytes()) != crc {
        return (Request::SilentDrop, V2Ctx::default());
    }

    // body re-split: /\s*(\S+)\s*(.*)/ -> cmd, raw-arg.
    let Some((cmd, raw_arg)) = split_body(body) else {
        return (Request::SilentDrop, V2Ctx::default());
    };

    let ctx = V2Ctx {
        reqid: reqid.to_string(),
    };

    // node: want = base64_decode(m2[2]); a decode failure yields null which
    // downstream treats as an empty/invalid arg. base64_decode('') === ''.
    let arg = base64_decode(raw_arg);

    let req = match cmd {
        "GET" => Request::Get {
            key: arg.unwrap_or_default().trim().to_string(),
            is_v2: true,
        },
        "KEYS" => Request::Keys { is_v2: true },
        "PUT" => parse_put(raw_arg),
        "DELETE" => Request::Delete {
            // V2 DELETE arg is the already-decoded plaintext key, trimmed.
            key: arg.unwrap_or_default().trim().to_string(),
        },
        other => Request::Unknown {
            cmd: other.to_string(),
            is_v2: true,
        },
    };
    (req, ctx)
}

/// PUT has two space-separated base64 terms (key, value), each decoded.
fn parse_put(raw_arg: &str) -> Request {
    // node: terms = (want||'').trim().split(' '); must be exactly 2.
    let terms: Vec<&str> = raw_arg.trim().split(' ').collect();
    if terms.len() != 2 {
        // Signaled as an empty key so the resolver returns the
        // "Invalid PUT Request" error (node behavior).
        return Request::Put {
            key: String::new(),
            value: String::new(),
        };
    }
    let key = base64_decode(terms[0]).unwrap_or_default().trim().to_string();
    let value = base64_decode(terms[1]);
    match value {
        Some(v) => Request::Put { key, value: v },
        // value === null -> Invalid PUT Request (empty key sentinel).
        None => Request::Put {
            key: String::new(),
            value: String::new(),
        },
    }
}

/// Split `<clen> <crc> <reqid> <body>` with the leading `\s*` and the
/// character-class constraints of the node regex.
fn split_v2_fields(s: &str) -> Option<(&str, &str, &str, &str)> {
    let s = s.trim_start();
    let (clen, rest) = take_while_then_space(s, |c| c.is_ascii_digit())?;
    let (crc, rest) = take_while_then_space(rest, is_lower_hex)?;
    let (reqid, body) = take_while_then_space(rest, is_lower_hex)?;
    if clen.is_empty() || crc.is_empty() || reqid.is_empty() {
        return None;
    }
    Some((clen, crc, reqid, body))
}

/// Take a maximal run of `pred` chars, then require and skip one-or-more
/// whitespace, returning `(token, remainder)`. Mirrors `(<class>+)\s+`.
fn take_while_then_space(s: &str, pred: impl Fn(char) -> bool) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !pred(c)).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let token = &s[..end];
    let rest = &s[end..];
    let after_ws = rest.trim_start();
    if after_ws.len() == rest.len() {
        // No whitespace followed the token: the `\s+` in the node regex fails.
        return None;
    }
    Some((token, after_ws))
}

/// body re-split per node `/\s*(\S+)\s*(.*)/`.
fn split_body(s: &str) -> Option<(&str, &str)> {
    let s = s.trim_start();
    if s.is_empty() {
        return None;
    }
    match s.find(char::is_whitespace) {
        Some(i) => {
            let cmd = &s[..i];
            let rest = s[i..].trim_start();
            Some((cmd, rest))
        }
        None => Some((s, "")),
    }
}

fn is_lower_hex(c: char) -> bool {
    c.is_ascii_digit() || ('a'..='f').contains(&c)
}

/// node base64_decode: returns the decoded UTF-8 string, or None on error.
/// An empty input decodes to an empty string (not an error).
fn base64_decode(s: &str) -> Option<String> {
    if s.is_empty() {
        return Some(String::new());
    }
    B64.decode(s).ok().map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// The application-level result the resolver produces for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A successful value to return (already stringified).
    Success(String),
    /// The key/value was absent.
    NotFound,
    /// An error; `message` is surfaced as the V2 FAILURE body (V1 drops it).
    Failure { message: String },
}

/// Render a V1 response for `outcome` (returns the bytes including the
/// trailing terminator). `\n`-terminated tokens match the node agent exactly.
#[must_use]
pub fn format_v1(outcome: &Outcome) -> Vec<u8> {
    match outcome {
        Outcome::Success(val) => {
            // Dot-stuffing: every line starting with '.' gets an extra '.'.
            let stuffed = dot_stuff(val);
            format!("SUCCESS\n{stuffed}\n.\n").into_bytes()
        }
        Outcome::NotFound => b"NOTFOUND\n".to_vec(),
        Outcome::Failure { .. } => b"FAILURE\n".to_vec(),
    }
}

/// Render a V2 response frame for `outcome`. `reqid` comes from the request.
#[must_use]
pub fn format_v2(reqid: &str, outcome: &Outcome) -> Vec<u8> {
    let (code, body): (&str, Option<&str>) = match outcome {
        Outcome::Success(val) => ("SUCCESS", Some(val.as_str())),
        Outcome::NotFound => ("NOTFOUND", None),
        Outcome::Failure { message } => ("FAILURE", Some(message.as_str())),
    };
    frame_v2_response(reqid, code, body)
}

/// The literal V1 `V2_OK\n` / `FAILURE\n` for a NEGOTIATE.
#[must_use]
pub fn format_negotiate(ver: &str) -> Vec<u8> {
    if ver == "V2" {
        b"V2_OK\n".to_vec()
    } else {
        b"FAILURE\n".to_vec()
    }
}

/// The literal `invalid command\n`.
#[must_use]
pub fn invalid_command() -> Vec<u8> {
    b"invalid command\n".to_vec()
}

fn frame_v2_response(reqid: &str, code: &str, body: Option<&str>) -> Vec<u8> {
    // node: resp = reqid + ' ' + code; if (body) resp += ' ' + base64(body)
    let mut resp = format!("{reqid} {code}");
    if let Some(b) = body {
        // node frames a body whenever it is truthy; an empty string is falsy
        // in JS and is therefore omitted (NOTFOUND has no body; an empty
        // SUCCESS value also omits the body, matching `if (body)`).
        if !b.is_empty() {
            resp.push(' ');
            resp.push_str(&B64.encode(b.as_bytes()));
        }
    }
    let crc = crc32_calc(resp.as_bytes());
    format!("V2 {} {} {}\n", resp.len(), crc, resp).into_bytes()
}

/// Apply node's `retval.replace(/^\./mg, '..')`: a leading '.' on any line is
/// doubled.
fn dot_stuff(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut at_line_start = true;
    for ch in s.chars() {
        if at_line_start && ch == '.' {
            out.push('.');
        }
        out.push(ch);
        at_line_start = ch == '\n';
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_v1_get() {
        let (req, _) = parse_line("GET sdc:hostname");
        assert_eq!(
            req,
            Request::Get {
                key: "sdc:hostname".to_string(),
                is_v2: false
            }
        );
    }

    #[test]
    fn parses_v1_keys() {
        let (req, _) = parse_line("KEYS");
        assert_eq!(req, Request::Keys { is_v2: false });
    }

    #[test]
    fn empty_get_is_invalid() {
        let (req, _) = parse_line("GET");
        assert_eq!(req, Request::Invalid);
        let (req2, _) = parse_line("NEGOTIATE");
        assert_eq!(req2, Request::Invalid);
    }

    #[test]
    fn negotiate_v2() {
        let (req, _) = parse_line("NEGOTIATE V2");
        assert_eq!(
            req,
            Request::Negotiate {
                ver: "V2".to_string()
            }
        );
        assert_eq!(format_negotiate("V2"), b"V2_OK\n");
        assert_eq!(format_negotiate("V9"), b"FAILURE\n");
    }

    #[test]
    fn v1_put_is_unknown() {
        // PUT in V1 is not honored -> Unknown -> FAILURE.
        let (req, _) = parse_line("PUT a b");
        assert_eq!(
            req,
            Request::Unknown {
                cmd: "PUT".to_string(),
                is_v2: false
            }
        );
    }

    // Build a valid V2 request frame the way the node client does.
    fn make_v2(reqid: &str, cmd: &str, b64arg: &str) -> String {
        let body = if b64arg.is_empty() {
            cmd.to_string()
        } else {
            format!("{cmd} {b64arg}")
        };
        let inner = format!("{reqid} {body}");
        format!("V2 {} {} {}", inner.len(), crc32_calc(inner.as_bytes()), inner)
    }

    #[test]
    fn parses_v2_get() {
        let arg = B64.encode(b"sdc:hostname");
        let line = make_v2("1", "GET", &arg);
        // Matches the spec golden request frame exactly.
        assert_eq!(line, "V2 22 ebd19df9 1 GET c2RjOmhvc3RuYW1l");
        let (req, ctx) = parse_line(&line);
        assert_eq!(
            req,
            Request::Get {
                key: "sdc:hostname".to_string(),
                is_v2: true
            }
        );
        assert_eq!(ctx.reqid, "1");
    }

    #[test]
    fn parses_v2_keys_no_arg() {
        let line = make_v2("5", "KEYS", "");
        assert_eq!(line, "V2 6 87eb6efd 5 KEYS");
        let (req, ctx) = parse_line(&line);
        assert_eq!(req, Request::Keys { is_v2: true });
        assert_eq!(ctx.reqid, "5");
    }

    #[test]
    fn v2_bad_crc_is_silent_drop() {
        let inner = "1 GET c2RjOmhvc3RuYW1l";
        let line = format!("V2 {} deadbeef {}", inner.len(), inner);
        let (req, _) = parse_line(&line);
        assert_eq!(req, Request::SilentDrop);
    }

    #[test]
    fn v2_bad_len_is_silent_drop() {
        let inner = "1 GET c2RjOmhvc3RuYW1l";
        let line = format!("V2 99 {} {}", crc32_calc(inner.as_bytes()), inner);
        let (req, _) = parse_line(&line);
        assert_eq!(req, Request::SilentDrop);
    }

    #[test]
    fn parses_v2_put() {
        let k = B64.encode(b"mykey");
        let v = B64.encode(b"myval");
        let line = make_v2("9", "PUT", &format!("{k} {v}"));
        let (req, _) = parse_line(&line);
        assert_eq!(
            req,
            Request::Put {
                key: "mykey".to_string(),
                value: "myval".to_string()
            }
        );
    }

    #[test]
    fn parses_v2_delete() {
        let k = B64.encode(b"mykey");
        let line = make_v2("9", "DELETE", &k);
        let (req, _) = parse_line(&line);
        assert_eq!(
            req,
            Request::Delete {
                key: "mykey".to_string()
            }
        );
    }

    #[test]
    fn v1_success_dot_stuffs() {
        let out = format_v1(&Outcome::Success("img2".to_string()));
        assert_eq!(out, b"SUCCESS\nimg2\n.\n");
        // A value line that is just '.' becomes '..'.
        let out2 = format_v1(&Outcome::Success(".\nfoo".to_string()));
        assert_eq!(out2, b"SUCCESS\n..\nfoo\n.\n");
    }

    #[test]
    fn v1_notfound_failure() {
        assert_eq!(format_v1(&Outcome::NotFound), b"NOTFOUND\n");
        assert_eq!(
            format_v1(&Outcome::Failure {
                message: "x".to_string()
            }),
            b"FAILURE\n"
        );
    }

    #[test]
    fn v2_success_frame_matches_live() {
        // reqid 000001, value "img2" -> exact live bytes.
        let out = format_v2("000001", &Outcome::Success("img2".to_string()));
        assert_eq!(out, b"V2 23 0cb469ba 000001 SUCCESS aW1nMg==\n");
    }

    #[test]
    fn v2_notfound_frame_matches_live() {
        let out = format_v2("000002", &Outcome::NotFound);
        assert_eq!(out, b"V2 15 1ddcd766 000002 NOTFOUND\n");
    }

    #[test]
    fn v2_keys_frame_matches_live() {
        let out = format_v2("000003", &Outcome::Success("root_pw".to_string()));
        assert_eq!(out, b"V2 27 40a81145 000003 SUCCESS cm9vdF9wdw==\n");
    }

    #[test]
    fn v2_failure_carries_message() {
        let out = format_v2(
            "7",
            &Outcome::Failure {
                message: "Invalid PUT Request".to_string(),
            },
        );
        let s = String::from_utf8(out).expect("utf8");
        assert!(s.starts_with("V2 "));
        assert!(s.contains("FAILURE"));
        // body is base64 of the message.
        let b64 = B64.encode(b"Invalid PUT Request");
        assert!(s.contains(&b64), "frame {s} lacks {b64}");
    }
}
