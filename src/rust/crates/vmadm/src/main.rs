// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! `vmadm` — native Rust drop-in for the SmartOS vmadm CLI. The command +
//! option surface matches node `vmadm` (so existing scripts keep working):
//! `create -f`, `stop -F -t`, `list -p -H -o -s field=value`,
//! `lookup -j -1 -o field=value`, etc. Scope: `bhyve` + `joyent-minimal`.

mod fields;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use tokio::io::AsyncReadExt as _;
use tritond_vmadm::backend::{HostBackend, HostNicTagRegistrar};
use tritond_vmadm::payload::{VmadmCreatePayload, normalize};

#[derive(Parser)]
#[command(
    name = "vmadm",
    about = "Manage bhyve and joyent-minimal zones (native Rust vmadm)",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create a new VM from a JSON payload on stdin (or `-f <file>`).
    Create {
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Boot a stopped VM. Trailing `option=value` boot params are accepted
    /// for compatibility (ignored for OS zones).
    Start {
        uuid: String,
        #[arg(value_name = "option=value")]
        options: Vec<String>,
    },
    /// Shut a VM down (`-F` forces, `-t` sets the shutdown timeout).
    Stop {
        uuid: String,
        #[arg(short = 'F', long)]
        force: bool,
        #[arg(short = 't', long)]
        timeout: Option<u64>,
    },
    /// Reboot a VM (`-F` forces, `-t` sets the shutdown timeout).
    Reboot {
        uuid: String,
        #[arg(short = 'F', long)]
        force: bool,
        #[arg(short = 't', long)]
        timeout: Option<u64>,
    },
    /// Delete a VM and its storage.
    Delete { uuid: String },
    /// Connect to the VM's console (`zlogin -C`; Ctrl-] to disconnect).
    Console { uuid: String },
    /// Reprovision a VM from a new image (`-f <file>` / stdin payload).
    Reprovision {
        uuid: String,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
    },
    /// Print a VM's JSON.
    Get { uuid: String },
    /// List VMs. Table by default; `-p` parsable, `-H` no header,
    /// `-o`/`-s` columns/sort, trailing `field=value` filters, `-j` JSON.
    List {
        #[arg(short = 'p', long)]
        parsable: bool,
        #[arg(short = 'H', long = "no-header")]
        no_header: bool,
        #[arg(short = 'o', long = "order")]
        order: Option<String>,
        #[arg(short = 's', long = "sortby")]
        sortby: Option<String>,
        #[arg(short = 'j', long)]
        json: bool,
        #[arg(value_name = "field=value")]
        filters: Vec<String>,
    },
    /// Look up VMs by `field=value`; prints matching uuids (`-j` JSON,
    /// `-1` require exactly one, `-o` print fields).
    Lookup {
        #[arg(short = 'j', long)]
        json: bool,
        #[arg(short = '1', long = "unique")]
        unique: bool,
        #[arg(short = 'o', long = "output")]
        output: Option<String>,
        #[arg(value_name = "field=value")]
        filters: Vec<String>,
    },
    /// Update a VM (`-f <file>` / stdin payload or trailing
    /// `property=value`). The native engine supports disk resize.
    Update {
        uuid: String,
        #[arg(short = 'f', long = "file")]
        file: Option<PathBuf>,
        #[arg(value_name = "property=value")]
        properties: Vec<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Create { file } => create(file).await,
        Command::Start { uuid, options } => {
            if !options.is_empty() {
                eprintln!("vmadm: start boot options are ignored for OS zones: {options:?}");
            }
            run_control("started", &uuid, |u| async move {
                tritond_vmadm::lifecycle::start(&HostBackend::new(), u).await
            })
            .await
        }
        Command::Stop {
            uuid,
            force,
            timeout,
        } => {
            let t = Duration::from_secs(timeout.unwrap_or(180));
            run_control("stopped", &uuid, move |u| async move {
                tritond_vmadm::lifecycle::stop(&HostBackend::new(), u, force, t).await
            })
            .await
        }
        Command::Reboot { uuid, force, .. } => {
            run_control("rebooted", &uuid, move |u| async move {
                tritond_vmadm::lifecycle::reboot(&HostBackend::new(), u, force).await
            })
            .await
        }
        Command::Delete { uuid } => {
            run_control("deleted", &uuid, |u| async move {
                tritond_vmadm::lifecycle::delete(&HostBackend::new(), u).await
            })
            .await
        }
        Command::Reprovision { uuid, file } => reprovision(&uuid, file).await,
        Command::Console { uuid } => console(&uuid).await,
        Command::Get { uuid } => get(&uuid).await,
        Command::List {
            parsable,
            no_header,
            order,
            sortby,
            json,
            filters,
        } => list(parsable, no_header, order, sortby, json, filters).await,
        Command::Lookup {
            json,
            unique,
            output,
            filters,
        } => lookup(json, unique, output, filters).await,
        Command::Update {
            uuid,
            file,
            properties,
        } => update(&uuid, file, properties).await,
    }
}

/// Parse the uuid arg and run a lifecycle action, mapping the result to an
/// exit code.
async fn run_control<F, Fut>(verb: &str, uuid: &str, f: F) -> ExitCode
where
    F: FnOnce(uuid::Uuid) -> Fut,
    Fut: std::future::Future<Output = tritond_vmadm::Result<()>>,
{
    let parsed = match uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(e) => return fail(&format!("invalid uuid {uuid:?}: {e}")),
    };
    match f(parsed).await {
        Ok(()) => {
            println!("Successfully {verb} VM {parsed}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("{e}")),
    }
}

async fn create(file: Option<PathBuf>) -> ExitCode {
    let bytes = match read_payload(file).await {
        Ok(b) => b,
        Err(e) => return fail(&format!("reading payload: {e}")),
    };
    let payload = match VmadmCreatePayload::from_json_slice(&bytes) {
        Ok(p) => p,
        Err(e) => return fail(&format!("invalid create payload: {e}")),
    };
    let now = chrono::Utc::now();
    let spec = match normalize(payload, now) {
        Ok(s) => s,
        Err(e) => return fail(&format!("invalid create payload: {e}")),
    };
    match tritond_vmadm::create(&HostBackend::new(), &HostNicTagRegistrar::new(), &spec, now).await
    {
        Ok(uuid) => {
            println!("Successfully created VM {uuid}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("{e}")),
    }
}

async fn reprovision(uuid: &str, file: Option<PathBuf>) -> ExitCode {
    let parsed = match uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(e) => return fail(&format!("invalid uuid {uuid:?}: {e}")),
    };
    let bytes = match read_payload(file).await {
        Ok(b) => b,
        Err(e) => return fail(&format!("reading reprovision payload: {e}")),
    };
    let payload: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => return fail(&format!("invalid reprovision payload: {e}")),
    };
    let Some(image) = payload
        .get("image_uuid")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<uuid::Uuid>().ok())
    else {
        return fail("reprovision payload missing image_uuid");
    };
    let zpool = payload
        .get("zpool")
        .and_then(|v| v.as_str())
        .unwrap_or("zones");
    match tritond_vmadm::lifecycle::reprovision(&HostBackend::new(), parsed, image, zpool).await {
        Ok(()) => {
            println!("Successfully reprovisioned VM {parsed}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("{e}")),
    }
}

/// `vmadm console <uuid>` — hand the TTY to the zone console. The zone
/// must be running. Both supported brands (joyent-minimal + bhyve, whose
/// `com1=/dev/zconsole`) use `zlogin -C`; the escape sequence Ctrl-]
/// (`\035`) disconnects. `exec` replaces this process so the console is
/// fully interactive; it only returns on failure.
async fn console(uuid: &str) -> ExitCode {
    use std::os::unix::process::CommandExt as _;
    use tritond_vmadm::ZoneHost as _;

    let parsed = match uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(e) => return fail(&format!("invalid uuid {uuid:?}: {e}")),
    };
    let info = match HostBackend::new().zone_info(parsed).await {
        Ok(Some(i)) => i,
        Ok(None) => return fail(&format!("no such VM {parsed}")),
        Err(e) => return fail(&format!("{e}")),
    };
    if !info.is_running() {
        return fail(&format!(
            "cannot connect to console when state is {:?}; must be running",
            info.state
        ));
    }
    let err = std::process::Command::new("/usr/sbin/zlogin")
        .args(["-C", "-e", "\\035", &parsed.to_string()])
        .exec();
    fail(&format!("exec zlogin -C: {err}"))
}

/// `vmadm get <uuid>` — print the VM JSON.
async fn get(uuid: &str) -> ExitCode {
    let parsed = match uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(e) => return fail(&format!("invalid uuid {uuid:?}: {e}")),
    };
    match tritond_vmadm::lifecycle::get(&HostBackend::new(), parsed).await {
        Ok(Some(vm)) => print_json(&vm),
        Ok(None) => fail(&format!("no such VM {parsed}")),
        Err(e) => fail(&format!("{e}")),
    }
}

async fn list(
    parsable: bool,
    no_header: bool,
    order: Option<String>,
    sortby: Option<String>,
    json: bool,
    filters: Vec<String>,
) -> ExitCode {
    let mut vms = match collect(&filters).await {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    if json {
        return print_json(&vms);
    }
    let sortby = sortby.as_deref().unwrap_or(fields::DEFAULT_SORT);
    fields::sort_vms(&mut vms, sortby);
    let order = order.as_deref().unwrap_or(fields::DEFAULT_ORDER);
    print!("{}", fields::format_list(&vms, order, !no_header, parsable));
    ExitCode::SUCCESS
}

async fn lookup(
    json: bool,
    unique: bool,
    output: Option<String>,
    filters: Vec<String>,
) -> ExitCode {
    let mut vms = match collect(&filters).await {
        Ok(v) => v,
        Err(e) => return fail(&e),
    };
    fields::sort_vms(&mut vms, fields::DEFAULT_SORT);
    if unique && vms.len() != 1 {
        return fail(&format!(
            "lookup matched {} VMs, expected exactly 1",
            vms.len()
        ));
    }
    if json {
        return print_json(&vms);
    }
    if let Some(order) = output {
        // `-o`: print the requested fields per match, no header.
        print!("{}", fields::format_list(&vms, &order, false, false));
    } else {
        let uuids = fields::format_lookup_uuids(&vms);
        if !uuids.is_empty() {
            println!("{uuids}");
        }
    }
    ExitCode::SUCCESS
}

/// Fetch all VMs and apply the `field=value` filters.
async fn collect(filters: &[String]) -> Result<Vec<tritond_vmadm::VmGetJson>, String> {
    let vms = tritond_vmadm::lifecycle::list(&HostBackend::new())
        .await
        .map_err(|e| e.to_string())?;
    Ok(vms
        .into_iter()
        .filter(|vm| fields::matches_filters(vm, filters))
        .collect())
}

async fn update(uuid: &str, file: Option<PathBuf>, properties: Vec<String>) -> ExitCode {
    let parsed = match uuid.parse::<uuid::Uuid>() {
        Ok(u) => u,
        Err(e) => return fail(&format!("invalid uuid {uuid:?}: {e}")),
    };
    // Build the update payload from -f/stdin JSON or `property=value` args.
    let payload = if properties.is_empty() {
        let bytes = match read_payload(file).await {
            Ok(b) => b,
            Err(e) => return fail(&format!("reading update payload: {e}")),
        };
        match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(v) => v,
            Err(e) => return fail(&format!("invalid update payload: {e}")),
        }
    } else {
        serde_json::Value::Object(
            properties
                .iter()
                .filter_map(|p| p.split_once('='))
                .map(|(k, v)| (k.to_string(), serde_json::Value::String(v.to_string())))
                .collect(),
        )
    };

    // The native engine implements disk resize. Pull the boot-disk target
    // from `update_disks` (MiB) or `flexible_disk_size`. `property=value`
    // args arrive JSON-encoded as strings, so accept a numeric string too.
    let as_num = |v: &serde_json::Value| -> Option<u64> {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
    };
    let new_mib = payload
        .get("update_disks")
        .and_then(|d| d.as_array())
        .and_then(|a| a.iter().find_map(|d| d.get("size").and_then(&as_num)))
        .or_else(|| payload.get("flexible_disk_size").and_then(&as_num));
    match new_mib {
        Some(mib) => {
            match tritond_vmadm::lifecycle::grow_boot_disk(
                &HostBackend::new(),
                parsed,
                mib * 1024 * 1024,
            )
            .await
            {
                Ok(()) => {
                    println!("Successfully updated VM {parsed}");
                    ExitCode::SUCCESS
                }
                Err(e) => fail(&format!("{e}")),
            }
        }
        None => {
            fail("native vmadm update supports disk resize only (update_disks/flexible_disk_size)")
        }
    }
}

async fn read_payload(file: Option<PathBuf>) -> std::io::Result<Vec<u8>> {
    match file {
        Some(path) => tokio::fs::read(path).await,
        None => {
            let mut buf = Vec::new();
            tokio::io::stdin().read_to_end(&mut buf).await?;
            Ok(buf)
        }
    }
}

fn print_json<T: serde::Serialize>(v: &T) -> ExitCode {
    match serde_json::to_string_pretty(v) {
        Ok(s) => {
            println!("{s}");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&format!("serialize: {e}")),
    }
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("vmadm: {msg}");
    ExitCode::FAILURE
}
