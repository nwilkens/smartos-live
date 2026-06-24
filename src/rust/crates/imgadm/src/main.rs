// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Native Rust drop-in for the SmartOS `imgadm` CLI.
//!
//! Scope (per the node-eradication plan): zone-dataset + zvol images
//! consumed from an IMGAPI v2 source. The docker and dsapi source
//! types and image production (`create`/`publish`) are out of scope.

mod config;
mod database;
mod error;
mod imageinfo;
mod source;
mod tool;
mod zfs;

use clap::{Args, Parser, Subcommand};
use serde_json::Value;

use config::DEFAULT_ZPOOL;
use error::{Error, Result};
use imageinfo::{
    filters_from_args, matches_filters, print_table, row_from_image_info, ImageInfo,
};
use tool::Tool;

#[derive(Parser)]
#[command(name = "imgadm", version, disable_help_subcommand = true)]
struct Cli {
    /// Verbose output: trace-level logging, stack on error.
    #[arg(short = 'v', long = "verbose", global = true)]
    verbose: bool,

    /// On error, emit a structured JSON error object on stderr.
    #[arg(short = 'E', global = true)]
    structured_error: bool,

    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List and edit image sources.
    Sources(SourcesArgs),
    /// List available images from all sources.
    #[command(alias = "available")]
    Avail(ListLikeArgs),
    /// Show the manifest for an available image.
    Show(ShowArgs),
    /// List locally installed images.
    List(ListArgs),
    /// Get information for an installed image.
    #[command(alias = "info")]
    Get(GetArgs),
    /// List the ancestry (origin chain) for an image.
    Ancestry(AncestryArgs),
    /// Import an image from a source IMGAPI.
    Import(ImportArgs),
    /// Install an image from a local manifest and file.
    Install(InstallArgs),
    /// Delete an installed image.
    #[command(alias = "destroy")]
    Delete(DeleteArgs),
    /// Refresh cached manifest info from sources.
    Update(UpdateArgs),
}

#[derive(Args)]
struct SourcesArgs {
    /// JSON output.
    #[arg(short = 'j', long = "json")]
    json: bool,
    /// Verbose (table) output.
    #[arg(short = 'v', long = "verbose")]
    verbose: bool,
    /// Add a source.
    #[arg(short = 'a')]
    add: Option<String>,
    /// Delete a source.
    #[arg(short = 'd')]
    del: Option<String>,
    /// Ping-check all sources.
    #[arg(short = 'c', long = "check")]
    check: bool,
    /// Source type for an added source.
    #[arg(short = 't', long = "type", default_value = "imgapi")]
    ty: String,
    /// Allow insecure (no TLS verify) access to an added HTTPS source.
    #[arg(short = 'k', long = "insecure")]
    insecure: bool,
    /// Skip the ping-check when adding a source.
    #[arg(short = 'f', long = "force")]
    force: bool,
}

#[derive(Args)]
struct ListLikeArgs {
    #[arg(short = 'j', long = "json")]
    json: bool,
    #[arg(short = 'H')]
    no_header: bool,
    #[arg(short = 'o', default_value = "uuid,name,version,os,type,pub")]
    columns: String,
    #[arg(short = 's', default_value = "published_at,name")]
    sort: String,
    /// FIELD=VALUE filters.
    filters: Vec<String>,
}

#[derive(Args)]
struct ListArgs {
    #[arg(short = 'j', long = "json")]
    json: bool,
    #[arg(short = 'H')]
    no_header: bool,
    #[arg(short = 'o', default_value = "uuid,name,version,os,type,pub")]
    columns: String,
    #[arg(short = 's', default_value = "published_at,name")]
    sort: String,
    filters: Vec<String>,
}

#[derive(Args)]
struct ShowArgs {
    uuid: String,
}

#[derive(Args)]
struct GetArgs {
    /// Recursively gather children (snapshots and clones).
    #[arg(short = 'r')]
    recursive: bool,
    #[arg(short = 'P', default_value = DEFAULT_ZPOOL)]
    pool: String,
    uuid: String,
}

#[derive(Args)]
struct AncestryArgs {
    #[arg(short = 'j', long = "json")]
    json: bool,
    #[arg(short = 'H')]
    no_header: bool,
    #[arg(short = 'o', default_value = "uuid,name,version,published")]
    columns: String,
    #[arg(short = 'P', default_value = DEFAULT_ZPOOL)]
    pool: String,
    uuid: String,
}

#[derive(Args)]
struct ImportArgs {
    #[arg(short = 'C', long = "channel")]
    channel: Option<String>,
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
    #[arg(short = 'P', default_value = DEFAULT_ZPOOL)]
    pool: String,
    /// Override configured sources (forces type imgapi).
    #[arg(short = 'S', long = "source")]
    source: Vec<String>,
    /// Source sends a raw ZFS stream (ignore files[0] checksum/size).
    #[arg(long = "zstream")]
    zstream: bool,
    image_id: String,
}

#[derive(Args)]
struct InstallArgs {
    #[arg(short = 'm')]
    manifest: String,
    #[arg(short = 'f')]
    file: String,
    #[arg(short = 'P', default_value = DEFAULT_ZPOOL)]
    pool: String,
    #[arg(short = 'q', long = "quiet")]
    quiet: bool,
    #[arg(long = "zstream")]
    zstream: bool,
}

#[derive(Args)]
struct DeleteArgs {
    #[arg(short = 'P', default_value = DEFAULT_ZPOOL)]
    pool: String,
    uuid: String,
}

#[derive(Args)]
struct UpdateArgs {
    #[arg(short = 'n', long = "dry-run")]
    dry_run: bool,
    uuids: Vec<String>,
}

fn main() {
    let cli = Cli::parse();
    let structured = cli.structured_error;
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("imgadm: error: {e}");
            std::process::exit(1);
        }
    };
    let code = match rt.block_on(run(cli)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("imgadm: error: {e}");
            if structured {
                let obj = serde_json::json!({ "message": e.to_string() });
                eprintln!("{obj}");
            }
            1
        }
    };
    std::process::exit(code);
}

async fn run(cli: Cli) -> Result<()> {
    match cli.cmd {
        Command::Sources(a) => cmd_sources(a).await,
        Command::Avail(a) => cmd_avail(a).await,
        Command::Show(a) => cmd_show(a).await,
        Command::List(a) => cmd_list(a),
        Command::Get(a) => cmd_get(a),
        Command::Ancestry(a) => cmd_ancestry(a),
        Command::Import(a) => cmd_import(a).await,
        Command::Install(a) => cmd_install(a).await,
        Command::Delete(a) => cmd_delete(a),
        Command::Update(a) => cmd_update(a).await,
    }
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

// ---- sources ----

async fn cmd_sources(a: SourcesArgs) -> Result<()> {
    let mut tool = Tool::load()?;

    let nactions =
        a.add.is_some() as u8 + a.del.is_some() as u8 + a.check as u8;
    if nactions > 1 {
        return Err(Error::Usage(
            "cannot specify more than one of \"-a\", \"-d\", and \"-c\"".to_string(),
        ));
    }

    if let Some(url) = a.add {
        let src = config::Source {
            ty: a.ty.clone(),
            url: url.clone(),
            insecure: if a.insecure { Some(true) } else { None },
            channel: None,
        };
        if !a.force && src.ty == "imgapi" {
            let client = source::ImgapiClient::new(&src, &tool.user_agent)?;
            if let Err(e) = client.ping().await {
                return Err(Error::Internal(format!(
                    "could not ping {}: {e}",
                    src.url
                )));
            }
        }
        let display = src.display();
        if tool.config.add_source(src) {
            tool.config.save()?;
            println!("Added {display}");
        } else {
            println!("Already have {display}, no change");
        }
        return Ok(());
    }

    if let Some(url) = a.del {
        let deleted = tool.config.del_source_url(&url);
        if deleted.is_empty() {
            println!("Do not have image source \"{url}\", no change");
        } else {
            tool.config.save()?;
            for s in deleted {
                println!("Deleted {}", s.display());
            }
        }
        return Ok(());
    }

    if a.check {
        let mut rows = Vec::new();
        for s in tool.sources() {
            let mut row = serde_json::Map::new();
            row.insert("url".to_string(), Value::String(s.url.clone()));
            row.insert("type".to_string(), Value::String(s.ty.clone()));
            let ping = if s.ty == "imgapi" {
                match source::ImgapiClient::new(s, &tool.user_agent) {
                    Ok(c) => c.ping().await,
                    Err(e) => Err(e),
                }
            } else {
                Ok(())
            };
            match ping {
                Ok(()) => {
                    row.insert("ok".to_string(), Value::Bool(true));
                }
                Err(e) => {
                    row.insert("ok".to_string(), Value::Bool(false));
                    row.insert("error".to_string(), Value::String(e.to_string()));
                }
            }
            rows.push(row);
        }
        let cols = vec![
            "url".to_string(),
            "type".to_string(),
            "ok".to_string(),
            "error".to_string(),
        ];
        print_table(&rows, &cols, &[], false);
        return Ok(());
    }

    // Default listing.
    if a.json {
        // node builds `{url, type, insecure}` then JSON.stringify drops
        // `insecure` when it is undefined, so omit it when None.
        let arr: Vec<Value> = tool
            .sources()
            .iter()
            .map(|s| {
                let mut m = serde_json::Map::new();
                m.insert("url".to_string(), Value::String(s.url.clone()));
                m.insert("type".to_string(), Value::String(s.ty.clone()));
                if let Some(i) = s.insecure {
                    m.insert("insecure".to_string(), Value::Bool(i));
                }
                Value::Object(m)
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    } else if a.verbose {
        let rows: Vec<serde_json::Map<String, Value>> = tool
            .sources()
            .iter()
            .map(|s| {
                let mut m = serde_json::Map::new();
                m.insert("url".to_string(), Value::String(s.url.clone()));
                m.insert("type".to_string(), Value::String(s.ty.clone()));
                m.insert(
                    "insecure".to_string(),
                    match s.insecure {
                        Some(b) => Value::Bool(b),
                        None => Value::Null,
                    },
                );
                m
            })
            .collect();
        let cols = vec!["url".to_string(), "type".to_string(), "insecure".to_string()];
        print_table(&rows, &cols, &[], false);
    } else {
        for s in tool.sources() {
            println!("{}", s.url);
        }
    }
    Ok(())
}

// ---- avail ----

async fn cmd_avail(a: ListLikeArgs) -> Result<()> {
    let tool = Tool::load()?;
    if tool.sources().is_empty() {
        return Err(Error::NoSources);
    }
    let filters = filters_from_args(&a.filters)?;
    let (infos, err) = tool.sources_list().await;
    let kept: Vec<ImageInfo> = infos
        .into_iter()
        .filter(|i| matches_filters(i, &filters))
        .collect();
    emit_list(
        &kept,
        a.json,
        &split_csv(&a.columns),
        &split_csv(&a.sort),
        a.no_header,
    )?;
    // Match node: still print results, then surface a source error.
    if let Some(e) = err {
        return Err(e);
    }
    Ok(())
}

// ---- show ----

async fn cmd_show(a: ShowArgs) -> Result<()> {
    let tool = Tool::load()?;
    if !zfs::is_uuid(&a.uuid) {
        return Err(Error::ImageNotFound(a.uuid));
    }
    match tool
        .sources_get_import_info(&a.uuid, None, false)
        .await?
    {
        Some((manifest, _source)) => {
            println!("{}", serde_json::to_string_pretty(&manifest)?);
            Ok(())
        }
        None => Err(Error::ImageNotFound(a.uuid)),
    }
}

// ---- list ----

fn cmd_list(a: ListArgs) -> Result<()> {
    let tool = Tool::load()?;
    let filters = filters_from_args(&a.filters)?;
    let infos = tool.list_images()?;
    let kept: Vec<ImageInfo> = infos
        .into_iter()
        .filter(|i| matches_filters(i, &filters))
        .collect();
    emit_list(
        &kept,
        a.json,
        &split_csv(&a.columns),
        &split_csv(&a.sort),
        a.no_header,
    )
}

// ---- get ----

fn cmd_get(a: GetArgs) -> Result<()> {
    let tool = Tool::load()?;
    if !zfs::is_uuid(&a.uuid) {
        return Err(Error::Usage(format!("invalid uuid: \"{}\"", a.uuid)));
    }
    match tool.get_image(&a.uuid, &a.pool, a.recursive)? {
        Some(info) => {
            println!("{}", serde_json::to_string_pretty(&info.to_value())?);
            Ok(())
        }
        None => Err(Error::ImageNotInstalled { uuid: a.uuid }),
    }
}

// ---- ancestry ----

fn cmd_ancestry(a: AncestryArgs) -> Result<()> {
    let tool = Tool::load()?;
    if !zfs::is_uuid(&a.uuid) {
        return Err(Error::Usage(format!("invalid uuid: \"{}\"", a.uuid)));
    }
    let mut chain: Vec<ImageInfo> = Vec::new();
    let mut cur = a.uuid.clone();
    loop {
        let info = tool
            .get_image(&cur, &a.pool, false)?
            .ok_or(Error::ImageNotInstalled { uuid: cur.clone() })?;
        let origin = info.origin().map(str::to_string);
        chain.push(info);
        match origin {
            Some(o) => cur = o,
            None => break,
        }
    }
    // Ancestry uses no sort (insertion order).
    emit_list(&chain, a.json, &split_csv(&a.columns), &[], a.no_header)
}

// ---- import ----

async fn cmd_import(a: ImportArgs) -> Result<()> {
    let tool = Tool::load()?;
    if !zfs::is_uuid(&a.image_id) {
        return Err(Error::Usage(format!(
            "invalid image-id arg: \"{}\"",
            a.image_id
        )));
    }
    let uuid = a.image_id.clone();

    // Already installed?
    if let Some(ii) = tool.get_image(&uuid, &a.pool, false)? {
        let extra = match (ii.mstr("name"), ii.mstr("version")) {
            (Some(n), Some(v)) => format!(" ({n} {v})"),
            _ => String::new(),
        };
        println!("Image {uuid}{extra} is already installed, skipping");
        return Ok(());
    }

    // Resolve sources (override with -S).
    let override_sources: Option<Vec<config::Source>> = if a.source.is_empty() {
        None
    } else {
        Some(
            a.source
                .iter()
                .map(|u| config::Source {
                    ty: "imgapi".to_string(),
                    url: u.clone(),
                    insecure: None,
                    channel: a.channel.clone(),
                })
                .collect(),
        )
    };

    let resolved = tool
        .sources_get_import_info(&uuid, override_sources.as_deref(), true)
        .await?;
    let Some((manifest, source)) = resolved else {
        return Err(Error::ActiveImageNotFound(uuid));
    };

    tool.import_image(manifest, &source, &a.pool, a.zstream, |m| println!("{m}"))
        .await
}

// ---- install ----

async fn cmd_install(a: InstallArgs) -> Result<()> {
    let tool = Tool::load()?;
    let manifest_path = std::path::Path::new(&a.manifest);
    let file_path = std::path::Path::new(&a.file);
    if !manifest_path.exists() {
        return Err(Error::Usage(format!(
            "manifest path \"{}\" does not exist",
            a.manifest
        )));
    }
    if !file_path.exists() {
        return Err(Error::Usage(format!(
            "file path \"{}\" does not exist",
            a.file
        )));
    }
    let bytes = std::fs::read(manifest_path)?;
    let mut manifest: Value = serde_json::from_slice(&bytes)?;
    // node sets published_at to now if missing.
    if manifest.get("published_at").is_none()
        && let Value::Object(m) = &mut manifest
    {
        let now = chrono::Utc::now()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
        m.insert("published_at".to_string(), Value::String(now));
    }
    tool.install_image(manifest, file_path, &a.pool, a.zstream, |m| {
        println!("{m}")
    })
    .await
}

// ---- delete ----

fn cmd_delete(a: DeleteArgs) -> Result<()> {
    let tool = Tool::load()?;
    if !zfs::is_uuid(&a.uuid) {
        return Err(Error::Usage(format!("invalid uuid: \"{}\"", a.uuid)));
    }
    tool.delete_image(&a.uuid, &a.pool)?;
    println!("Deleted image {}", a.uuid);
    Ok(())
}

// ---- update ----

async fn cmd_update(a: UpdateArgs) -> Result<()> {
    let tool = Tool::load()?;
    tool.update_images(&a.uuids, a.dry_run, |m| println!("{m}"))
        .await
}

// ---- shared list emit ----

fn emit_list(
    infos: &[ImageInfo],
    json: bool,
    columns: &[String],
    sort: &[String],
    no_header: bool,
) -> Result<()> {
    if json {
        let arr: Vec<Value> = infos.iter().map(ImageInfo::to_value).collect();
        println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    } else {
        let rows: Vec<serde_json::Map<String, Value>> =
            infos.iter().map(row_from_image_info).collect();
        print_table(&rows, columns, sort, no_header);
    }
    Ok(())
}
