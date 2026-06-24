// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The IMGADM backend tool: the operations behind each subcommand.

use std::io::Write;
use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use imgapi_manifest::{Compression, Manifest};
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::config::{Config, Source, DEFAULT_ZPOOL};
use crate::database;
use crate::error::{Error, Result};
use crate::imageinfo::ImageInfo;
use crate::source::ImgapiClient;
use crate::zfs;

const DOWNLOAD_DIR: &str = "/var/tmp/.imgadm-downloads";
const LOCK_DIR: &str = "/var/run";

pub struct Tool {
    pub config: Config,
    pub user_agent: String,
}

impl Tool {
    pub fn load() -> Result<Tool> {
        let config = Config::load()?;
        let user_agent = format!("imgadm/{}", env!("CARGO_PKG_VERSION"));
        Ok(Tool { config, user_agent })
    }

    pub fn sources(&self) -> &[Source] {
        &self.config.sources
    }

    fn client_for(&self, source: &Source) -> Result<ImgapiClient> {
        ImgapiClient::new(source, &self.user_agent)
    }

    // ---- enumeration ----

    /// All installed images with cached manifests merged in.
    pub fn list_images(&self) -> Result<Vec<ImageInfo>> {
        let installed = zfs::load_images()?;
        let mut out = Vec::with_capacity(installed.len());
        for img in installed {
            let mut info = database::load_image(&img.zpool, &img.uuid);
            info.clones = Some(img.clone_names.len() as u64);
            info.clone_names = Some(img.clone_names);
            out.push(info);
        }
        Ok(out)
    }

    /// One installed image (`get`). Returns None if not installed.
    pub fn get_image(
        &self,
        uuid: &str,
        zpool: &str,
        children: bool,
    ) -> Result<Option<ImageInfo>> {
        if !zfs::dataset_exists(zpool, uuid)? {
            return Ok(None);
        }
        let mut info = database::load_image(zpool, uuid);
        if children {
            info.children = Some(zfs::dataset_children(zpool, uuid)?);
        }
        Ok(Some(info))
    }

    // ---- remote (avail / show / import-info) ----

    /// Available images across all non-docker sources, first source
    /// wins on duplicate uuids. Returns (infos, first_error).
    pub async fn sources_list(&self) -> (Vec<ImageInfo>, Option<Error>) {
        let mut infos: Vec<ImageInfo> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut first_err: Option<Error> = None;

        for source in self.sources() {
            if source.ty == "docker" {
                continue;
            }
            let client = match self.client_for(source) {
                Ok(c) => c,
                Err(e) => {
                    first_err.get_or_insert(e);
                    continue;
                }
            };
            match client.list_images().await {
                Ok(images) => {
                    for manifest in images {
                        let uuid = manifest
                            .get("uuid")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if uuid.is_empty() || seen.contains(&uuid) {
                            continue;
                        }
                        seen.insert(uuid);
                        infos.push(ImageInfo {
                            manifest,
                            zpool: None,
                            source: Some(source.url.clone()),
                            clones: None,
                            clone_names: None,
                            children: None,
                            base: None,
                        });
                    }
                }
                Err(e) => {
                    first_err.get_or_insert(e);
                }
            }
        }
        (infos, first_err)
    }

    /// Find import info (the raw manifest) for `uuid` across sources.
    /// Returns the manifest and the source it came from.
    pub async fn sources_get_import_info(
        &self,
        uuid: &str,
        sources: Option<&[Source]>,
        ensure_active: bool,
    ) -> Result<Option<(Value, Source)>> {
        if !zfs::is_uuid(uuid) {
            return Ok(None);
        }
        let srcs: &[Source] = sources.unwrap_or_else(|| self.sources());
        if srcs.is_empty() {
            return Err(Error::NoSources);
        }
        let mut last_err: Option<Error> = None;
        for source in srcs {
            if source.ty == "docker" {
                continue;
            }
            let client = match self.client_for(source) {
                Ok(c) => c,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            match client.get_image(uuid).await {
                Ok(Some(manifest)) => {
                    let is_active = manifest.get("state").and_then(Value::as_str)
                        == Some("active");
                    if !ensure_active || is_active {
                        return Ok(Some((manifest, source.clone())));
                    }
                }
                Ok(None) => {}
                Err(e) => last_err = Some(e),
            }
        }
        if let Some(e) = last_err {
            return Err(e);
        }
        Ok(None)
    }

    // ---- delete ----

    pub fn delete_image(&self, uuid: &str, zpool: &str) -> Result<()> {
        let info = self.get_image(uuid, zpool, true)?;
        let Some(info) = info else {
            return Err(Error::ImageNotInstalled {
                uuid: uuid.to_string(),
            });
        };
        let clones = info
            .children
            .as_ref()
            .and_then(|c| c.get("clones"))
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        if clones > 0 {
            let names: Vec<String> = info
                .children
                .as_ref()
                .and_then(|c| c.get("clones"))
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            return Err(Error::ImageHasDependentClones {
                uuid: uuid.to_string(),
                clones: names.join(", "),
            });
        }
        zfs::destroy(zpool, uuid)?;
        database::delete_image(zpool, uuid)?;
        Ok(())
    }

    // ---- import / install ----

    /// Import an already-resolved manifest from a source: download +
    /// verify + receive + persist. `log` is called with progress.
    pub async fn import_image<F: FnMut(&str)>(
        &self,
        manifest: Value,
        source: &Source,
        zpool: &str,
        zstream: bool,
        mut log: F,
    ) -> Result<()> {
        let typed = Manifest::parse(serde_json::to_string(&manifest)?.as_bytes())
            .map_err(|e| Error::InvalidManifest(e.to_string()))?;
        let uuid = typed.uuid.to_string();

        log(&format!(
            "Importing {} ({}@{}) from \"{}\"",
            uuid, typed.name, typed.version, source.url
        ));

        // Per-uuid lock to serialize concurrent imports.
        let _lock = ImportLock::acquire(&uuid)?;

        // Re-check after acquiring the lock (race).
        if zfs::dataset_exists(zpool, &uuid)? {
            log(&format!(
                "Image {uuid} ({}@{}) was already installed",
                typed.name, typed.version
            ));
            return Ok(());
        }

        let file0 = typed
            .files
            .first()
            .ok_or_else(|| Error::InvalidManifest("manifest has no files".to_string()))?;
        let expected_size = file0.size;
        let expected_sha1 = file0.sha1.clone();
        let compression = file0.compression;

        let client = self.client_for(source)?;
        let file_path = download_file(&client, &uuid).await?;

        let result = self
            .finish_install(
                &file_path,
                &manifest,
                &uuid,
                zpool,
                Some(source),
                compression,
                zstream,
                if zstream { None } else { Some(expected_size) },
                if zstream { None } else { Some(expected_sha1.as_str()) },
            )
            .await;

        // Clean up the download regardless.
        let _ = std::fs::remove_file(&file_path);

        result?;
        log(&format!(
            "Imported image {uuid} ({}@{}) to \"{}/{uuid}\"",
            typed.name, typed.version, zpool
        ));
        Ok(())
    }

    /// Install from a local manifest + file (no download).
    pub async fn install_image<F: FnMut(&str)>(
        &self,
        manifest: Value,
        file_path: &Path,
        zpool: &str,
        zstream: bool,
        mut log: F,
    ) -> Result<()> {
        let typed = Manifest::parse(serde_json::to_string(&manifest)?.as_bytes())
            .map_err(|e| Error::InvalidManifest(e.to_string()))?;
        let uuid = typed.uuid.to_string();

        log(&format!(
            "Installing image {uuid} ({}@{})",
            typed.name, typed.version
        ));

        let _lock = ImportLock::acquire(&uuid)?;
        if zfs::dataset_exists(zpool, &uuid)? {
            log(&format!(
                "Image {uuid} ({}@{}) is already installed, skipping",
                typed.name, typed.version
            ));
            return Ok(());
        }

        let file0 = typed
            .files
            .first()
            .ok_or_else(|| Error::InvalidManifest("manifest has no files".to_string()))?;
        let compression = file0.compression;
        let expected_size = file0.size;
        let expected_sha1 = file0.sha1.clone();

        self.finish_install(
            file_path,
            &manifest,
            &uuid,
            zpool,
            None,
            compression,
            zstream,
            if zstream { None } else { Some(expected_size) },
            if zstream { None } else { Some(expected_sha1.as_str()) },
        )
        .await?;

        log(&format!(
            "Installed image {uuid} ({}@{})",
            typed.name, typed.version
        ));
        Ok(())
    }

    /// Verify + persist manifest + zfs receive. The manifest is saved
    /// to the db *before* the file so a half-install can be recovered
    /// (node ordering). On any receive error the db entry is removed.
    #[allow(clippy::too_many_arguments)]
    async fn finish_install(
        &self,
        file_path: &Path,
        manifest: &Value,
        uuid: &str,
        zpool: &str,
        source: Option<&Source>,
        compression: Compression,
        zstream: bool,
        expected_size: Option<u64>,
        expected_sha1: Option<&str>,
    ) -> Result<()> {
        // Verify size + sha1 (skipped for zstream).
        if let Some(size) = expected_size {
            let actual = std::fs::metadata(file_path)?.len();
            if actual != size {
                return Err(Error::Download(format!(
                    "unexpected size: expected {size}, got {actual}"
                )));
            }
        }
        if let Some(sha1) = expected_sha1 {
            let actual = sha1_of_file(file_path)?;
            if actual != sha1 {
                return Err(Error::Download(format!(
                    "unexpected sha1: expected {sha1}, got {actual}"
                )));
            }
        }

        // Persist manifest first.
        database::add_image(zpool, manifest, source.map(|s| s.url.as_str()))?;

        // Receive the dataset; on failure roll back the db entry.
        let recv = tokio::task::block_in_place(|| {
            zfs::install_zfs_image(file_path, zpool, uuid, compression, zstream)
        });
        if let Err(e) = recv {
            let _ = database::delete_image(zpool, uuid);
            return Err(e);
        }
        Ok(())
    }

    // ---- update ----

    /// Refresh cached manifests for installed images from sources.
    /// For each installed image without a full manifest (or all, if
    /// uuids is empty) re-fetch the manifest and persist it.
    pub async fn update_images<F: FnMut(&str)>(
        &self,
        uuids: &[String],
        dry_run: bool,
        mut log: F,
    ) -> Result<()> {
        let installed = self.list_images()?;
        for info in installed {
            let Some(uuid) = info.uuid().map(str::to_string) else {
                continue;
            };
            if !uuids.is_empty() && !uuids.contains(&uuid) {
                continue;
            }
            // If the cached manifest already has a name/version, treat
            // it as complete (node refreshes regardless; we refresh
            // only sparse entries to avoid clobbering live data).
            let has_full = info.mstr("name").is_some() && info.mstr("version").is_some();
            if has_full {
                continue;
            }
            let zpool = info.zpool.clone().unwrap_or_else(|| DEFAULT_ZPOOL.to_string());
            match self.sources_get_import_info(&uuid, None, false).await {
                Ok(Some((manifest, source))) => {
                    if dry_run {
                        log(&format!("[dry-run] Updated image {uuid}"));
                    } else {
                        database::add_image(&zpool, &manifest, Some(&source.url))?;
                        log(&format!("Updated image {uuid}"));
                    }
                }
                Ok(None) => {}
                Err(_) => {}
            }
        }
        Ok(())
    }
}

/// Download the image file to `<DOWNLOAD_DIR>/<uuid>.file`, returning
/// its path. Streams the body to disk.
async fn download_file(client: &ImgapiClient, uuid: &str) -> Result<PathBuf> {
    std::fs::create_dir_all(DOWNLOAD_DIR)?;
    let final_path = PathBuf::from(DOWNLOAD_DIR).join(format!("{uuid}.file"));
    let partial = PathBuf::from(DOWNLOAD_DIR).join(format!("{uuid}.file.partial"));

    let (_len, mut stream) = client.get_image_file_stream(uuid).await?;
    {
        let mut f = std::fs::File::create(&partial)?;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            f.write_all(&chunk)?;
        }
        f.flush()?;
    }
    std::fs::rename(&partial, &final_path)?;
    Ok(final_path)
}

/// Compute the lowercase hex sha1 of a file.
fn sha1_of_file(path: &Path) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    std::io::copy(&mut f, &mut hasher)?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// A per-uuid advisory lock file under /var/run. Serializes
/// concurrent imports of the same image (node `_lockAcquire`).
///
/// The `fd_lock::RwLock` owns the file fd; the OS flock is acquired
/// once and held by leaking the guard, so the lock stays held for as
/// long as this struct (and thus the fd) is alive. On drop the fd
/// closes, which releases the flock, and we remove the lock file.
struct ImportLock {
    _lock: fd_lock::RwLock<std::fs::File>,
    path: PathBuf,
}

impl ImportLock {
    fn acquire(uuid: &str) -> Result<ImportLock> {
        let path = PathBuf::from(LOCK_DIR).join(format!("img.{uuid}.import.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&path)?;
        let mut lock = fd_lock::RwLock::new(file);
        {
            let guard = lock
                .write()
                .map_err(|e| Error::Internal(format!("lock {}: {e}", path.display())))?;
            // Keep the OS flock held for the struct's lifetime; the fd
            // remains owned by `lock` and closing it on drop releases.
            std::mem::forget(guard);
        }
        Ok(ImportLock { _lock: lock, path })
    }
}

impl Drop for ImportLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}
