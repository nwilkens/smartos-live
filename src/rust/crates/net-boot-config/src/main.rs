// net-boot-config -- a Rust reimplementation of the SmartOS net-boot-config
// CLI (src/net-boot-config + node_modules/net-boot-config.js). Outputs the
// boot-time networking configuration; stdout is matched byte-for-byte against
// the node tool for all consumer-used modes (dump, --enabled, --routes,
// --nictag-rules, --json).

mod config;
mod stringify;

use std::process::ExitCode;

// dashdash-recognized options. Unknown -/-- tokens are an error; bare tokens
// (e.g. the no-op `grep <pat>` form used by sysinfo) are ignored positionals.
struct Opts {
    enabled: bool,
    json: bool,
    help: bool,
    nictag_rules: bool,
    routes: bool,
}

enum Parsed {
    Ok(Opts),
    // dashdash error: printed as "<me>: error: <msg>" then exit 1.
    Err(String),
}

fn parse_argv(argv: &[String]) -> Parsed {
    let mut opts = Opts {
        enabled: false,
        json: false,
        help: false,
        nictag_rules: false,
        routes: false,
    };

    for arg in argv.iter().skip(1) {
        if arg == "--" {
            // dashdash stops option parsing; the rest are positionals (ignored).
            break;
        }
        if let Some(long) = arg.strip_prefix("--") {
            // Long option, possibly --name=value; bool options reject a value.
            let (name, has_val) = match long.split_once('=') {
                Some((n, _)) => (n, true),
                None => (long, false),
            };
            match name {
                "enabled" => opts.enabled = true,
                "json" => opts.json = true,
                "help" => opts.help = true,
                "nictag-rules" => opts.nictag_rules = true,
                "routes" => opts.routes = true,
                _ => return Parsed::Err(format!("unknown option: \"--{name}\"")),
            }
            if has_val {
                return Parsed::Err(format!(
                    "argument given to \"--{name}\" option that does not take one"
                ));
            }
        } else if arg.starts_with('-') && arg.len() > 1 {
            // Short option group; only -j and -h are defined.
            for c in arg[1..].chars() {
                match c {
                    'j' => opts.json = true,
                    'h' => opts.help = true,
                    _ => {
                        return Parsed::Err(format!("unknown option: \"-{c}\""));
                    }
                }
            }
        }
        // else: positional arg, ignored (matches node main()).
    }

    Parsed::Ok(opts)
}

fn me(argv: &[String]) -> String {
    argv.first()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.clone())
        })
        .unwrap_or_else(|| "net-boot-config".to_string())
}

// Byte-exact dashdash help body for this option set (indent 4, helpCol 20).
const HELP_BODY: &str = concat!(
    "    --enabled       Exit 0 if boot-time networking is enabled.\n",
    "    -j, --json      Output JSON.\n",
    "    -h, --help      Print help and exit.\n",
    "    --nictag-rules  Print nic tag rules.\n",
    "    --routes        Print routes."
);

fn print_help(me: &str) {
    println!("usage: {me} [OPTIONS]");
    println!();
    println!("Options:");
    println!("{HELP_BODY}");
    println!();
    println!("If no options are specified, outputs config values");
}

fn run() -> ExitCode {
    let argv: Vec<String> = std::env::args().collect();
    let me = me(&argv);

    let opts = match parse_argv(&argv) {
        Parsed::Ok(o) => o,
        Parsed::Err(msg) => {
            eprintln!("{me}: error: {msg}");
            return ExitCode::from(1);
        }
    };

    if opts.help {
        print_help(&me);
        return ExitCode::SUCCESS;
    }

    if opts.enabled {
        return if config::enabled() {
            ExitCode::SUCCESS
        } else {
            ExitCode::from(1)
        };
    }

    if opts.json || opts.routes || opts.nictag_rules {
        let conf = match config::load_config() {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::SUCCESS;
            }
        };

        if opts.json {
            println!("{}", stringify::pretty(&serde_json::Value::Object(conf), 2));
            return ExitCode::SUCCESS;
        }
        if opts.routes {
            for (dst, gw) in config::routes(&conf) {
                println!("{dst} {gw}");
            }
            return ExitCode::SUCCESS;
        }
        // nictag-rules
        println!("{}", stringify::pretty(&config::nictag_rules(&conf), 2));
        return ExitCode::SUCCESS;
    }

    // Default: dump KEY=VALUE config values.
    let conf = match config::load_config() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::SUCCESS;
        }
    };
    for (k, v) in config::config_values(&conf) {
        println!("{k}={v}");
    }
    ExitCode::SUCCESS
}

fn main() -> ExitCode {
    run()
}
