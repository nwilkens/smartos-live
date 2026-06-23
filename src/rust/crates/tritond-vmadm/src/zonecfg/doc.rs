// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Low-level `zonecfg` command-stream emitter. Each method mirrors one
//! node `VM.js` code path so the rendered byte stream matches what
//! `vmadm` pipes to `zonecfg -z <zone>` on stdin. The quirks (unquoted
//! `provisioning` value, the `transition` attr's `name,value,type` order,
//! the comma-spacing difference between `add property` in the NIC loop and
//! the `primary` select lines) are reproduced deliberately.

use base64::Engine as _;

/// An accumulating `zonecfg` command stream.
#[derive(Debug, Default)]
pub struct Zonecfg {
    buf: String,
}

impl Zonecfg {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a verbatim line (a trailing newline is added).
    pub fn line(&mut self, s: &str) {
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    /// `set <prop>=<value>` (value unquoted, as node emits scalar sets).
    pub fn set(&mut self, prop: &str, value: &str) {
        self.line(&format!("set {prop}={value}"));
    }

    /// A standard string attr: `add attr; set name="<n>"; set type=string;
    /// set value="<v>"; end` (the `setAttr` form, name+value quoted).
    pub fn attr(&mut self, name: &str, value: &str) {
        self.line(&format!(
            "add attr; set name=\"{name}\"; set type=string; set value=\"{value}\"; end"
        ));
    }

    /// A base64-encoded string attr (alias / vnc-password / ...).
    pub fn attr_base64(&mut self, name: &str, raw: &str) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(raw.as_bytes());
        self.attr(name, &encoded);
    }

    /// The `provisioning` header attr: value is the bare token `true`, and
    /// node concatenates `value=true;` with `end` (no space):
    /// `add attr; set name="provisioning"; set type=string; set value=true;end`.
    pub fn attr_provisioning(&mut self) {
        self.line("add attr; set name=\"provisioning\"; set type=string; set value=true;end");
    }

    /// The `transition` attr (`VM.js` `buildTransitionZonecfg`): name is
    /// unquoted and the field order is name, value, type:
    /// `add attr; set name=transition; set value="<t>:<target>:<ms>"; set type=string; end`.
    pub fn attr_transition(&mut self, transition: &str, target: &str, expire_ms: i64) {
        self.line(&format!(
            "add attr; set name=transition; set value=\"{transition}:{target}:{expire_ms}\"; set type=string; end"
        ));
    }

    /// `add capped-memory; set physical=<p>m; set locked=<l>m; set swap=<s>m; end`.
    pub fn capped_memory(&mut self, physical_mib: u64, locked_mib: u64, swap_mib: u64) {
        self.line(&format!(
            "add capped-memory; set physical={physical_mib}m; set locked={locked_mib}m; set swap={swap_mib}m; end"
        ));
    }

    /// `add capped-cpu; set ncpus=<n>; end`. `ncpus` is `cpu_cap/100`
    /// rendered without trailing zeros to match node's `(cap*0.01)`.
    pub fn capped_cpu(&mut self, cpu_cap: u32) {
        self.line(&format!(
            "add capped-cpu; set ncpus={}; end",
            ncpus(cpu_cap)
        ));
    }

    /// Append a fully-formed device block (built by the caller).
    pub fn push_block(&mut self, block: &str) {
        self.buf.push_str(block);
    }

    #[must_use]
    pub fn render(self) -> String {
        self.buf
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.buf
    }
}

/// Render `cpu_cap/100` the way node's `(cpu_cap * 0.01).toString()` does
/// for the common (integer) cases: a whole number of vCPUs prints without
/// a decimal, a half prints as `N.5`.
fn ncpus(cpu_cap: u32) -> String {
    if cpu_cap.is_multiple_of(100) {
        (cpu_cap / 100).to_string()
    } else {
        // Trim trailing zeros from the fractional rendering.
        let v = f64::from(cpu_cap) / 100.0;
        let s = format!("{v}");
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provisioning_attr_has_no_space_before_end() {
        let mut z = Zonecfg::new();
        z.attr_provisioning();
        assert_eq!(
            z.as_str(),
            "add attr; set name=\"provisioning\"; set type=string; set value=true;end\n"
        );
    }

    #[test]
    fn transition_attr_order_is_name_value_type() {
        let mut z = Zonecfg::new();
        z.attr_transition("provisioning", "running", 1000);
        assert_eq!(
            z.as_str(),
            "add attr; set name=transition; set value=\"provisioning:running:1000\"; set type=string; end\n"
        );
    }

    #[test]
    fn capped_memory_one_line() {
        let mut z = Zonecfg::new();
        z.capped_memory(2048, 2048, 4096);
        assert_eq!(
            z.as_str(),
            "add capped-memory; set physical=2048m; set locked=2048m; set swap=4096m; end\n"
        );
    }

    #[test]
    fn ncpus_integer_and_fraction() {
        assert_eq!(ncpus(200), "2");
        assert_eq!(ncpus(100), "1");
        assert_eq!(ncpus(50), "0.5");
        assert_eq!(ncpus(150), "1.5");
    }

    #[test]
    fn alias_attr_is_base64() {
        let mut z = Zonecfg::new();
        z.attr_base64("alias", "web01");
        // base64("web01") == "d2ViMDE="
        assert_eq!(
            z.as_str(),
            "add attr; set name=\"alias\"; set type=string; set value=\"d2ViMDE=\"; end\n"
        );
    }
}
