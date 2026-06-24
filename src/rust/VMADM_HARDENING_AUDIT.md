# vmadm hardening audit — findings & status

Adversarial audit of the native `tritond-vmadm` engine vs node `VM.js` (52 candidates → 36
confirmed → ~20 distinct issues). All confirmed items below are fixed and, where it mattered,
live-verified on a real compute node (engine path: create/delete/reprovision/grow against real
zones; control-plane zones untouched, baseline restored).

| ID | Sev | Issue | Status |
|----|-----|-------|--------|
| C1 | Critical | JM reprovision destroyed the delegated `/data` dataset (data loss) | Fixed — park-aside + dataset-resource remove/re-add; live: sentinel md5 byte-identical |
| H1 | High | bhyve lost its `limit_priv` lockdown (priv-esc) | Fixed — brand default `["default",-file_link_any,-net_access,-proc_fork,-proc_info,-proc_session]`; live-verified |
| H2 | High | `grow_boot_disk` bumped volsize before quota; no rollback; + boot-disk quote bug | Fixed — quota pre-bump first, full rollback; unquote `set match="…"`; live: grow + absurd-grow rollback |
| H3/L7 | High | unchecked MiB→bytes multiply wrapped (`refreservation=0`, `max-shm=0`) | Fixed — `checked_mul` + reject; `refreservation<=quota` |
| H4 | High | `zfs destroy -rf` aborts on transient EBUSY | Fixed — `-rF` (retry-on-busy) |
| M1 | Med | #1167 class: no `@indestructible` holds guard on delete/reprovision | Fixed — live `zfs holds` gate before any halt; `VmadmError::Indestructible` wired; live regression-verified |
| M2 | Med | reprovision left stale `image_uuid`/`dataset-uuid` | Fixed — updates device image-uuid (bhyve) / dataset-uuid attr (JM); live-verified |
| M3 | Med | bhyve ignored explicit `max_physical_memory` | Fixed |
| M4 | Med | `vnc_port`/`boot` attrs dropped | Fixed (KVM-only attrs intentionally excluded) |
| M5 | Med | `alias` raw-interpolated into NoCloud YAML (cloud-init injection) | Fixed — alias guarded + escaped YAML scalar |
| M6 | Med | stderr "tolerate" matched substring anywhere | Fixed — line-anchored, command-scoped |
| reprovision race | High* | internal stop returned before zone fully halted → uninstall on running zone | Fixed — wait for terminal `installed` state; live-verified |
| L1–L11 | Low | validation/parity: compression allow-list, nic_tag charset, resolvers IP, `halt -X`, max_swap floor, no-bootable-disk fail-closed, reject `archive_on_delete`/`internal_metadata_namespaces`, checkDatasets, graceful reprovision stop | Fixed |

\* surfaced by live testing of the fixes, not the original audit.

## Residual gaps (tracked, data-safe)
- JM delegate reprovision: surviving delegated dataset isn't auto-remounted in-zone (data intact; needs next-boot mount or explicit remount for full parity).
- `delegated_is_standard` only probes `/data`(+`/data2`); no list-child-datasets primitive yet, so a wholly non-standard delegated name isn't detected (it refuses on the cases it can see).
- `zonecfg_remove_dataset`/`add_dataset` tolerate phrases are best-effort; confirm exact stderr on the target for strict crash-retry idempotency.

Tests: 129 unit + 2 golden passing; production clippy clean (no `unwrap`/`expect` in non-test code).
