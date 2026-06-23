// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The create resource journal. Each mutating create step records its undo
//! action and is persisted (atomic write) BEFORE the side effect runs, so
//! the create path can roll the whole create back in reverse on any
//! failure. On a clean rollback the journal file is removed; it is RETAINED
//! only when an undo step itself failed, as a residue marker on disk
//! (`<dir>/<uuid>.journal`) for diagnosis. There is not yet an
//! agent-startup pass that replays a retained journal — that is tracked as
//! a follow-up; today a retained journal means "this zone is dirty, inspect
//! it" and the caller is also told via [`crate::error::VmadmError::RollbackResidue`].
//!
//! This module is pure data + persistence; the create path executes the
//! [`UndoAction`]s against its host (the journal does not run commands).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Result, VmadmError};

/// Default journal directory on a CN.
pub const JOURNAL_DIR: &str = "/var/run/vmadm";

/// What to do to undo a create step. Executed in reverse by the create
/// path's rollback (and by `recover` on a later agent start).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UndoAction {
    /// `zfs destroy -rf <dataset>` (tolerate "does not exist").
    ZfsDestroyRecursive { dataset: String },
    /// `zoneadm -u <uuid> uninstall -F` (tolerate already-uninstalled).
    ZoneadmUninstallForce { uuid: Uuid },
    /// `zonecfg -u <uuid> delete -F` (tolerate already-gone).
    ZonecfgDeleteForce { uuid: Uuid },
    /// Remove a file/marker.
    RemovePath { path: PathBuf },
    /// Nothing to undo (e.g. a read-only step recorded for ordering).
    Noop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Planned,
    Started,
    Done,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Human label for the step (e.g. "zoneroot", "zone-config").
    pub kind: String,
    pub state: StepState,
    pub undo: UndoAction,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceJournal {
    pub uuid: Uuid,
    pub entries: Vec<JournalEntry>,
    pub finished: bool,
    #[serde(skip)]
    path: PathBuf,
}

impl ResourceJournal {
    /// Open (create) a journal for `uuid` under `dir`, persisting an empty
    /// journal immediately.
    pub async fn open(uuid: Uuid, dir: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| VmadmError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        let path = dir.join(format!("{uuid}.journal"));
        let j = Self {
            uuid,
            entries: Vec::new(),
            finished: false,
            path,
        };
        j.persist().await?;
        Ok(j)
    }

    /// Record a planned step (persisted before the caller runs the side
    /// effect) and return its index.
    pub async fn plan(&mut self, kind: &str, undo: UndoAction) -> Result<usize> {
        self.entries.push(JournalEntry {
            kind: kind.to_string(),
            state: StepState::Planned,
            undo,
        });
        let idx = self.entries.len() - 1;
        self.persist().await?;
        Ok(idx)
    }

    /// Mark a step started (its side effect is now in flight, so undo must
    /// run on rollback).
    pub async fn mark_started(&mut self, idx: usize) -> Result<()> {
        self.set_state(idx, StepState::Started).await
    }

    /// Mark a step completed.
    pub async fn mark_done(&mut self, idx: usize) -> Result<()> {
        self.set_state(idx, StepState::Done).await
    }

    async fn set_state(&mut self, idx: usize, state: StepState) -> Result<()> {
        if let Some(e) = self.entries.get_mut(idx) {
            e.state = state;
        }
        self.persist().await
    }

    /// The undo actions to run on rollback, in reverse order: every step
    /// that started but did not complete cleanly. A `Done` step's resource
    /// is part of the (now to-be-removed) machine, so it is undone too — a
    /// failed create rolls the whole thing back.
    #[must_use]
    pub fn rollback_actions(&self) -> Vec<UndoAction> {
        self.entries
            .iter()
            .rev()
            .filter(|e| matches!(e.state, StepState::Started | StepState::Done))
            .map(|e| e.undo.clone())
            .collect()
    }

    /// Mark the create complete and remove the journal file (success
    /// path).
    pub async fn finish_ok(mut self) -> Result<()> {
        self.finished = true;
        // Best-effort removal; a leftover finished journal is harmless.
        let _ = tokio::fs::remove_file(&self.path).await;
        Ok(())
    }

    async fn persist(&self) -> Result<()> {
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| VmadmError::Payload(format!("serialize journal: {e}")))?;
        crate::files::write_and_rename(&self.path, &json).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn uuid() -> Uuid {
        "11111111-1111-1111-1111-111111111111".parse().unwrap()
    }

    #[tokio::test]
    async fn rollback_is_reverse_of_started_and_done() {
        let dir = tempfile::tempdir().unwrap();
        let mut j = ResourceJournal::open(uuid(), dir.path()).await.unwrap();

        let i0 = j
            .plan(
                "zoneroot",
                UndoAction::ZfsDestroyRecursive {
                    dataset: "zones/x".into(),
                },
            )
            .await
            .unwrap();
        j.mark_started(i0).await.unwrap();
        j.mark_done(i0).await.unwrap();

        let i1 = j
            .plan(
                "zone-config",
                UndoAction::ZonecfgDeleteForce { uuid: uuid() },
            )
            .await
            .unwrap();
        j.mark_started(i1).await.unwrap();
        // i1 never completes (simulating a failure at install).

        let i2 = j
            .plan(
                "zone-install",
                UndoAction::ZoneadmUninstallForce { uuid: uuid() },
            )
            .await
            .unwrap();
        // i2 only planned, never started -> not in rollback set.
        let _ = i2;

        let actions = j.rollback_actions();
        assert_eq!(
            actions,
            vec![
                UndoAction::ZonecfgDeleteForce { uuid: uuid() },
                UndoAction::ZfsDestroyRecursive {
                    dataset: "zones/x".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn journal_persists_and_finish_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{}.journal", uuid()));
        let mut j = ResourceJournal::open(uuid(), dir.path()).await.unwrap();
        j.plan("x", UndoAction::Noop).await.unwrap();
        assert!(path.exists());

        // The persisted file round-trips.
        let raw = std::fs::read_to_string(&path).unwrap();
        let loaded: ResourceJournal = serde_json::from_str(&raw).unwrap();
        assert_eq!(loaded.uuid, uuid());
        assert_eq!(loaded.entries.len(), 1);

        j.finish_ok().await.unwrap();
        assert!(!path.exists());
    }
}
