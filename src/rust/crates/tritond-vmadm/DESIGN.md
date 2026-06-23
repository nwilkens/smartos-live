# tritond-vmadm — native Rust replacement for SmartOS node `vmadm`

## Why

`/usr/sbin/vmadm` on a SmartOS compute node is node.js (`vmadm.js` + the
658 KB `VM.js`, baked into the Platform Image). Triton vNext does not use
node `vmadm`'s *logic* — it drives the binary: `tritonagent` builds the
node-vmadm JSON create payload and `exec`s `vmadm create/start/stop/...`,
and `tritonadm setup apply` `exec`s `vmadm create` to stand up the
control-plane zones. This crate replaces that binary with a native Rust
engine so the PI no longer needs the node runtime for VM management.

Scope is vNext-first: the `bhyve` brand (IaaS guests) and the
`joyent-minimal` brand (native zones for control-plane services). No
`kvm`, `lx`, `joyent`, `docker`, `builder`.

## Shape

```
libs/tritond-vmadm   the engine library (this crate)
cli/vmadm            a thin drop-in `vmadm`-named binary over the engine
```

The engine's single create input is the **node-vmadm-compatible JSON
payload** (`payload::VmadmCreatePayload`). Both fronts converge on it:

- the drop-in `vmadm` binary reads it from stdin (node contract), and
- `tritonagent` already builds exactly this JSON today
  (`services/tritonagent/src/vmadm.rs` `build_create_payload*`); it stops
  `exec`ing and calls the engine in-process.

Because the agent already supplies its own `cloud-init:network-config`
(and `user-data`/`meta-data`) in `customer_metadata`, the engine's seed
builder passes provided `cloud-init:*` keys through verbatim and only
*auto-generates* a NoCloud netplan when a key is absent. There is no need
to host two netplan flavors in the engine.

## Brand mechanics that the engine must reproduce (ground-truthed in source)

Reference checkouts:
- `triton-vnext/smartos-live` (master) — complete `VM.js` + the
  vNext-authored `node_modules/cloudinit/{nocloud,lofs-fat16}.js`.
- `triton/smartos-live` (branch `proteus-nic-tag`) — the `VM.js` patch
  that accepts `proteus[N]` links as valid nic_tags. We accept proteus
  links by construction (see nic-tag, below), so that patch is moot here.
- `triton/illumos-joyent/usr/src/lib/brand/{jcommon,joyent-minimal,bhyve}`
  — the brand boot hooks.

### OS zones (`joyent-minimal`)

`VM.js` does **not** clone the image itself. It pipes a zonecfg blob to
`zonecfg -z <zonename>` (stdin), then `zoneadm -z <zonename> install -q
<quota_GiB> -t <image_uuid> -x nodataset`; the brand's `jinstall` does
`zfs clone zones/<image_uuid>@final zones/<zonename>` and applies the
quota. OS-zone quota is the `-q` flag **only** — there is no `zfs set
quota`/`refquota`/`refreservation` for OS zones (that is bhyve-only).

The brand boot hook (`jcommon/statechange` `setup_net`) creates the
in-zone vnic at boot: `dladm create-vnic -t -l <link> <tmp>` then
`dladm rename-link <tmp> net0`. It resolves `<link>` from the zonecfg
`global-nic` (the nic_tag) via sysinfo's tag→link map *or*
`dladm show-{phys,etherstub}`. A Proteus kmod pseudo-link is neither a
phys NIC nor a real etherstub, so it resolves **only if registered as a
nic_tag before boot** — otherwise `setup_net` bails `exit 1`
"undefined VNIC" and the zone never boots. (bhyve/viona attaches the raw
link directly and never hits this path — that is why bhyve "just works"
and joyent-minimal is fragile.)

### bhyve quota math (RFD 0148 snapspace) — verified in `VM.js` `setQuotaBhyve`

On `zones/<uuid>` (root) and `zones/<uuid>/disk<N>` (volumes):

```
maxQuota = max(DEFAULT_BHYVE_QUOTA=1, quota_gib) * 1 GiB        (bytes)
flexsize = flexible_disk_size_mib * 1 MiB  (0 if unset)         (bytes)

1. zfs set refreservation=none refquota=<maxQuota>  zones/<uuid>
2. zfs list -Hrpt volume -o name,volsize zones/<uuid>   -> volsize = Σ volsize
3. for each volume: zfs set refreservation=auto <vol>
4. zfs list -Hpr -t volume -o refreservation zones/<uuid> -> volrefres = Σ refreservation
5. quota = (flexsize != 0) ? volrefres + maxQuota + flexsize - volsize
                           : volrefres + maxQuota
   if (quota_gib * 1 GiB > quota) quota = "<quota_gib>g"     // override
6. zfs set quota=<quota> reservation=<quota>
          refreservation=max(0, maxQuota - 1 MiB)  zones/<uuid>
```

Constants (`proptable.js`/`VM.js`): `BHYVE_MEM_OVERHEAD = 1024+256 = 1280`,
`KVM_MEM_OVERHEAD = 1024`, `DEFAULT_BHYVE_QUOTA = 1`, `DEFAULT_QUOTA = 10`,
`PROVISION_TIMEOUT = 300`. bhyve `max_physical_memory = ram + 1280`.

### NoCloud seed disk — verified in vNext `cloudinit/{nocloud,lofs-fat16}.js`

A 16 MiB virtio zvol formatted FAT16 labeled `CIDATA`, added as the last
non-boot disk; `flexible_disk_size += 16` (or `quota += 1 GiB`). Files:
`meta-data` (always), `network-config` (if present), `user-data`
(defaults to `#cloud-config\n{}\n`), `vendor-data` (if present).

The vNext module builds it with **native tools** (not a FAT library):

```
mktemp -d /tmp/lofs-fat16-XXXXXX
lofiadm -a <zvol>                       -> /dev/lofi/N ; raw /dev/rlofi/N
yes | mkfs -F pcfs -o b=CIDATA,nofdisk,fat=16,size=32768  /dev/rlofi/N
mount -F pcfs -o rw /dev/lofi/N <mnt>
write each file into <mnt>
umount <mnt> ; rmdir <mnt> ; lofiadm -d /dev/lofi/N
```

**Decision:** the engine reproduces this **native** path by default
(proven guest-readable on the deployed PI, byte-compatible with node
vmadm). The lofi/mount cleanup is recorded in the resource journal. A
`fatfs`-crate builder is kept behind a `SeedBuilder` trait seam for a
future no-shellout option, but is **not** the default (a `fatfs` image is
not bit-identical to `mkfs_pcfs` and guest-readability would need
re-proving).

## The proteus nic-tag fix (the headline gotcha)

Registration of a Proteus link as a nic_tag currently lives in
`tritonagent/src/lib.rs` (`ensure_proteus_nic_tag`), runs best-effort in
the network-realize path, and a freshly-provisioned port sometimes was
not registered before the zone booted — so joyent-minimal boot bailed.

Fix, owned by the create path (the component that boots the zone owns the
precondition):

- `NicTagRegistrar` trait. `HostNicTagRegistrar::ensure(link)` is the
  existing logic verbatim — `nictagadm exists -l` short-circuit →
  `nictagadm add -l` → on the `etherstub creation failed: object already
  exists` EEXIST (the kmod already did `dls_devnet_create`) fall back to
  appending the link to `/tmp/.nic-tags` + `sysinfo -u`.
- The create path calls `ensure` for every NIC link **immediately before
  `zoneadm boot`**, and the failure policy is **brand-aware**:
  - `joyent-minimal`: an unregistered link is a **hard error before
    boot** (fail-closed) — never a warning that surfaces later as a
    mysterious "undefined VNIC".
  - `bhyve`: best-effort + warn (viona tolerates the raw link).
- Guard against emitting `proteus0` (the legacy `VM.js`
  `isProteusLinkNicTag` regex rejects a zero low-32 link id).

## Crate module map

```
src/
  lib.rs            re-exports; pub Vmadm facade
  error.rs          VmadmError (thiserror) + rest_code() parity
  constants.rs      the verified constants above + NoCloud (16 MiB/512/CIDATA/32768)
  brand.rs          Brand{JoyentMinimal,Bhyve} + per-brand features
  payload/
    mod.rs
    compat.rs       VmadmCreatePayload (node JSON shape, serde) — the engine input
    normalize.rs    normalizePayload + applyZoneDefaults (defaults, mac gen, primary, mem)
  model/
    spec.rs         VmCreateSpec (normalized internal model the engine consumes)
    disk.rs         DiskSpec, DiskModel, DiskMedia, Refreservation
    nic.rs          NicSpec + ruin_mac/fix_mac
    get_json.rs     VmGetJson (get/list output; satisfies tritonagent VmInfo/VmDisk)
  zonecfg/
    command.rs      ZoneCfgCommand/Resource/Attr enums + Display (byte-exact render)
    document.rs     ZoneCfgDocument { Vec<ZoneCfgCommand> }.render()
    build.rs        build_zonecfg_create()/build_zonecfg_update()
  quota.rs          compute_bhyve_quota() pure fn (RFD 0148) + applier
  seed/
    nocloud.rs      seed-file assembly (stock auto-gen fallback)
    pcfs.rs         native lofiadm/mkfs/mount seed builder (default)
  zfs_ext.rs        create_zvol -V, clone -F, destroy -rf, list volumes, dry-run refres
  zoneadm_ext.rs    install/boot/halt/uninstall argv
  files.rs          config dir, write+rename, metadata.json/tags.json/routes.json, markers
  nictag.rs         NicTagRegistrar trait + HostNicTagRegistrar (the proteus fix)
  journal/          ResourceJournal: write+fsync before each step; reverse-undo rollback
  lock.rs           per-zone flock(2) (libc), the only unsafe; isolated
  backend/
    traits.rs       ZoneBackend + NicTagRegistrar
    host.rs         HostBackend (production: zfs/zoneadm/zonecfg/svccfg/sysinfo/lofi)
    fake.rs #[test] in-memory backend recording argv + faking host state
  lifecycle/
    create.rs start.rs stop.rs reboot.rs delete.rs update.rs reprovision.rs get.rs
  engine.rs         Vmadm<B: ZoneBackend, N: NicTagRegistrar>
```

## Crash-safety

- per-zone `flock(LOCK_EX)` on `/var/run/vm.<uuid>.config.lockfile` for
  the whole of create/delete/update/reprovision (kernel releases it if a
  holder crashes — no stale-lockfile footgun).
- resource journal `/var/run/vmadm/<uuid>.journal`: write+fsync the undo
  action *before* each mutating exec, mark done after. On error, replay
  undo in reverse. On daemon start, finish any journal with no
  `finished_at` (interrupted create). Unifies the rollback rule node
  `VM.js` only half-implements: zonecfg present → `zoneadm uninstall -F`
  + `zonecfg delete -F`; absent → `zfs destroy -rf zones/<uuid>`.
- idempotent delete: tolerate "dataset does not exist"/"already halted";
  never destroy a `@final` origin; refuse on `indestructible_*`.

## Differential test harness (`cli/vmadm-difftest`, `feature=difftest`)

On a CN that still has node vmadm, run node vs native on the same payload
and diff: the `zonecfg` stdin blob (byte-exact), the ordered
zfs/zoneadm/zonecfg argv, the resulting `/etc/zones/<uuid>.xml`
(canonicalized), the full `zfs get -Hp` property set, the on-disk
`config/{metadata,tags,routes}.json` + netfiles, `vmadm get` JSON, and
(bhyve) the seed FAT label/filenames/bytes. Normalize uuids/macs/
timestamps before compare.

## Slices

V-0 skeleton+types · V-1 zonecfg builder+golden · V-2 zfs/zoneadm ext +
quota · V-3 files+journal+lock · V-4 joyent-minimal create→boot
(FakeBackend) · V-5 drop-in binary (JM) · V-6 get/list reconstruction ·
V-7 HostBackend + live JM smoke · V-8 bhyve create · V-9 NoCloud seed ·
V-10 start/stop/reboot/delete parity · V-11 update/reprovision · V-12 cut
tritonagent+tritonadm over, retire node shim · V-DIFF differential
harness (incremental, finalized at V-12).

Build/test on the cross-build host (142.147.4.194), never the Mac.

## Open hardening items (from the codex review; address before/with V-7 live)

- **Zoneroot path safety (`assertSafeZonePath`)**: netfiles/config writes go
  to `<zonepath>/root/...`; a malicious image could plant a symlink there.
  vNext images are operator-imported (trusted-ish), but add an
  `O_NOFOLLOW`/`assertSafeZonePath` equivalent before V-7 ships to be CTF-safe.
- **`/usbkey/config` vs `/tmp/.nic-tags`**: `HostNicTagRegistrar` relies on
  `nictagadm add -l` (which writes the active config) and only falls back to
  `/tmp/.nic-tags` on the kmod-EEXIST path, matching the agent. Confirm the
  active-config path on a USB-config CN; the re-verify (`nictagadm exists`)
  catches a silent miss.
- **Stale `/.zonecontrol/metadata.sock` + zoneinit cleanup**: node removes
  these from imported images; the native path skips them. Fine for clean
  joyent-minimal images — verify on first real boot.
- **Journal recovery consumer**: a create that crashes mid-flight leaves a
  retained journal; wire an agent-startup `recover::scan` to complete the
  rollback (currently the journal is retained but not auto-replayed).
