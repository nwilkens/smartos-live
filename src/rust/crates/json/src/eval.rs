// JS execution for -e/-c via boa_engine. json.js uses node's `vm` module with
// the datum object as the sandbox: its keys become globals, `this` is the
// datum, bare assignments add keys to the datum, and mutations reflect back.
// We reproduce that contract by seeding the boa global object with the datum's
// keys and diffing the global namespace afterward to recover the result.

use anyhow::{Context as _, Result, anyhow};
use boa_engine::{Context, Source};
use serde_json::Value;

// Snapshot of builtin global property names captured from a pristine context,
// used to subtract builtins when recovering the sandbox object after -e.
fn builtin_globals() -> Result<Vec<String>> {
    let mut ctx = Context::default();
    let v = ctx
        .eval(Source::from_bytes(
            "JSON.stringify(Object.getOwnPropertyNames(globalThis))",
        ))
        .map_err(|e| anyhow!("{e}"))?;
    let s = v.as_string().ok_or_else(|| anyhow!("bad builtin list"))?;
    let names: Vec<String> = serde_json::from_str(&s.to_std_string_escaped())?;
    Ok(names)
}

// Seed globalThis with the datum's keys so that bare identifiers, `this.key`
// (this===globalThis at top level), and new bare assignments all alias the
// same sandbox, matching node's `vm.runInNewContext(datum)`.
fn seed_context(datum: &Value) -> Result<Context> {
    let mut ctx = Context::default();
    let datum_json = serde_json::to_string(datum)?;
    let setup = format!(
        "(function() {{ var __d = JSON.parse({}); if (__d && typeof __d === 'object' && !Array.isArray(__d)) {{ for (var __k in __d) {{ globalThis[__k] = __d[__k]; }} }} }})();",
        js_string(&datum_json)
    );
    ctx.eval(Source::from_bytes(&setup))
        .map_err(|e| anyhow!("{e}"))?;
    Ok(ctx)
}

// -c: returns true if the snippet is truthy for this datum. Bare field refs and
// `this.`-qualified refs both resolve via globalThis.
pub fn cond(datum: &Value, snippet: &str) -> Result<bool> {
    let mut ctx = seed_context(datum)?;
    let program = format!("(function() {{ return ({snippet}); }}).call(globalThis)");
    let val = ctx
        .eval(Source::from_bytes(&program))
        .map_err(|e| anyhow!("{e}"))
        .context("evaluating -c condition")?;
    Ok(val.to_boolean())
}

// -e: returns the (possibly mutated) sandbox object, including bare globals
// created by the snippet, matching node's vm sandbox behavior.
pub fn exec(datum: &Value, snippet: &str) -> Result<Value> {
    let builtins = builtin_globals()?;

    // Non-object datum: node binds `this` to it; snippet mutations go to
    // globals, but output is the datum itself. Run with `this`=datum.
    if !datum.is_object() {
        let mut ctx = Context::default();
        let datum_json = serde_json::to_string(datum)?;
        let program = format!(
            "(function() {{ {snippet}; return this; }}).call(JSON.parse({}))",
            js_string(&datum_json)
        );
        let v = ctx
            .eval(Source::from_bytes(&program))
            .map_err(|e| anyhow!("{e}"))
            .context("evaluating -e snippet")?;
        let out = v
            .to_json(&mut ctx)
            .map_err(|e| anyhow!("{e}"))
            .context("serializing -e result")?;
        return Ok(out);
    }

    let mut ctx = seed_context(datum)?;
    let program = format!("(function() {{ {snippet}; }}).call(globalThis); 'ok';");
    ctx.eval(Source::from_bytes(&program))
        .map_err(|e| anyhow!("{e}"))
        .context("evaluating -e snippet")?;

    let exclude_json = serde_json::to_string(&builtins)?;
    let collector = format!(
        "(function() {{ var ex = {exclude_json}; var skip = {{}}; for (var i=0;i<ex.length;i++) skip[ex[i]]=true; var out = {{}}; var names = Object.getOwnPropertyNames(globalThis); for (var j=0;j<names.length;j++) {{ var n = names[j]; if (!skip[n]) out[n] = globalThis[n]; }} return JSON.stringify(out); }})()"
    );
    let v = ctx
        .eval(Source::from_bytes(&collector))
        .map_err(|e| anyhow!("{e}"))
        .context("recovering -e result")?;
    let s = v
        .as_string()
        .ok_or_else(|| anyhow!("non-serializable -e result"))?;
    let out: Value = serde_json::from_str(&s.to_std_string_escaped())?;
    Ok(out)
}

fn js_string(s: &str) -> String {
    serde_json::to_string(&Value::String(s.to_string())).unwrap_or_else(|_| "\"\"".into())
}
