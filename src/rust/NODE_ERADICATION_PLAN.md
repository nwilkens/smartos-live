# Node.js Eradication Plan — smartos-live

> Goal: ship a SmartOS Platform Image with **no `/usr/node`, no node native addons, no node tools** — every Node.js tool replaced by a native illumos Rust binary built inside this repo (`src/rust/`).

> Generated from the `node-eradication-plan` discovery workflow (10 agents over smartos-live + mariana-trench + monitor-reef + live node 10.88.88.102).


## Executive summary

Goal: ship a SmartOS PI with NO /usr/node, NO node native addons, NO node tools. The node footprint is 5 operator CLIs (vmadm, imgadm, fwadm, vminfo, disklayout/mkzpool/json/bunyan), 3 SMF daemons (vmadmd, vminfod, metadata), the VM.js/vmload/fw.js engine libraries, 7 native addons (kstat, zonename, libuuid, expat, dtrace-provider, qlocker+fs-ext), and the externally-supplied node 0.10 runtime.

Strategy: stand up a hermetic Rust workspace inside smartos-live (`src/rust/`), targeting x86_64-unknown-illumos, vendored offline, driven by gmake->cargo. Land the highest-confidence, already-proven vnext code first (cli/vmadm + tritond-vmadm + the tritond-cn-platform smartos wrappers), then the standalone utility tools (json, bunyan, disklayout, mkzpool, filewait, ntp_config), then the from-scratch surface that vnext does NOT cover (vminfod cache+HTTP, metadata agent socket protocol, fwadm/ipf engine, vmadmd console/QMP daemon, imgadm), and only delete the node runtime + addons in the final phase once nothing requires them.

Hard reality the plan resolves explicitly: (1) vnext tritond-vmadm is bhyve + joyent-minimal ONLY. KVM/lx/docker are a hard error. The plan REQUIRES a brand-scope decision (see open_questions/blockers) and is structured so node can stay alive for unsupported brands behind a thin shim until those brands are rewritten or dropped. (2) The build host .194 is DOWN, so NOTHING can be compile-verified for illumos right now; every phase's verification is staged against the .194 host returning, with byte-exact golden diffs vs live node .102 as the acceptance gate. (3) vnext has ZERO Rust for vminfod, metadata, fwadm/ipf, the vmadmd daemon control surface, and most of imgadm; those are large rewrite-from-scratch phases, not lifts. (4) The shipped VM.js on .102 lacks the proteus change and is a newer base build than this checkout, so VM.js golden diffs must be taken against a freshly built node PI, not blindly against .102.

The eradication is sequenced in 11 phases: workspace bootstrap (P0), vmadm engine+CLI lift (P1), standalone leaf tools (P2), node-zfs/kstat/sysinfo wrappers (P3), vminfod rewrite (P4), metadata agent rewrite (P5), vmadmd daemon rewrite (P6), fwadm/ipf engine rewrite (P7), imgadm rewrite (P8), KVM/lx brand decision execution (P9), and finally node runtime + addon + Makefile/manifest deletion (P10).

## Build & verification loop

- **Build host** `142.147.4.194` (smartos-build1): illumos zone, native `cargo/rustc 1.94.1`, target `x86_64-unknown-illumos`, 343 GB free. Builds platform binaries directly (no cross-compile).
- **Validation node** `10.88.88.102`: live platform `joyent_20260622T191148Z`, node v0.10.26. Used read-only for golden output capture; Rust output is byte-diffed against it.
- **Phase 0 proven**: a hello binary built on `.194` runs natively on `.102` (matching ABI/libc). Loop: build on `.194` → diff vs golden from `.102`.


## Phases

### P0 — Rust workspace bootstrap inside smartos-live (hermetic illumos build, gmake->cargo)
- **approach**: mixed | **risk**: high | **effort**: L | **depends on**: —
- **components**: src/rust/ cargo workspace, vendored crate cache, src/rust/Makefile.cargo include, configure: add rust toolchain to proto.strap, x86_64-unknown-illumos target wiring
- **deliverables**:
  - New cargo workspace at src/rust/ with [workspace] resolver=2, edition=2024, profile.{dev,release} panic='abort', and [workspace.lints.clippy] unwrap_used=deny/expect_used=deny copied verbatim from mariana-trench Cargo.toml so lifted crates compile unchanged.
  - src/rust/.cargo/config.toml pinning build.target = "x86_64-unknown-illumos", [source.crates-io] replace-with = "vendored-sources", and [source.vendored-sources] directory = "vendor" for fully-offline builds.
  - src/rust/vendor/ populated via `cargo vendor` for the engine-path closure ONLY (anyhow, async-trait, base64, chrono, ipnetwork, libc, serde, serde_json, thiserror 2.0, tokio, tracing, uuid, clap 4.5, tempfile) plus toolchain crates; checked into the repo (smartos-live ships vendored deps, no network at build time).
  - src/rust/rust-toolchain.toml pinning a specific stable channel + the x86_64-unknown-illumos target so the build host uses an identical compiler.
  - configure: add the Rust toolchain to the build prerequisites. Either (a) extend the pkgsrc/illumos-extra adjunct in proto.strap to include rustc+cargo for the build SmartOS host, or (b) document an explicit `rustup target add x86_64-unknown-illumos` step. Decision flagged in open_questions.
  - src/Makefile.defs: add CARGO=$(NATIVEDIR)/usr/bin/cargo (or pkgsrc path), CARGO_TARGET=x86_64-unknown-illumos, RUST_BINDIR=src/rust/target/$(CARGO_TARGET)/release, and CARGO_FLAGS=--offline --locked --frozen.
  - src/rust/Makefile (gmake wrapper) with a single `cargo build --release --offline --locked` invocation gated on a phony target, plus a `rust_check` target running `cargo clippy -- -D warnings` and `cargo test`.
  - A throwaway 'hello-illumos' bin in the workspace to prove the toolchain + target + vendoring + gmake->cargo path end-to-end before any real code lands.
- **verification**:
  - On the .194 build host (when reachable): `gmake -C src/rust` builds the hello bin offline with the network disabled (verify with `unshare`/firewall or by removing ~/.cargo registry).
  - `file src/rust/target/x86_64-unknown-illumos/release/hello-illumos` reports an ELF 64-bit illumos/SunOS executable.
  - scp the hello bin to live node .102 and confirm it runs (sanity that the target triple + libc match the running platform joyent_20260622T191148Z).
  - `cargo build --offline --frozen` succeeds with no network (proves vendoring is complete).

### P1 — vmadm engine + CLI lift from vnext (bhyve + joyent-minimal), node vmadm kept for other brands
- **approach**: copy-from-vnext | **risk**: high | **effort**: XL | **depends on**: [0]
- **components**: cli/vmadm (Rust bin 'vmadm'), libs/tritond-vmadm, libs/tritond-cn-platform smartos::zoneadm only, src/vm/sbin/vmadm.js (node, retained as fallback shim under a renamed path)
- **deliverables**:
  - Copy libs/tritond-vmadm (7628 LOC), libs/tritond-cn-platform (only smartos/zoneadm.rs needed by the engine; either lift the whole crate or inline ZoneadmTool to drop the dep per vnext workspace_notes), and cli/vmadm (701 LOC) into src/rust/.
  - Replace every `{ workspace = true }` Cargo.toml stanza with the concrete versions from mariana-trench's central [workspace.dependencies] table (already captured: thiserror 2.0, tokio full, clap 4.5 derive+env, uuid v4/v5/serde, chrono serde, ipnetwork serde no-default, etc).
  - Brand dispatch shim: the Rust `vmadm` binary handles brand=bhyve|joyent-minimal natively; for any other brand (kvm/lx/docker/joyent-full) it execs the retained node engine at a renamed path (e.g. /usr/vm/sbin/vmadm-legacy) OR errors UnsupportedBrand, per the brand-scope decision in open_questions. This keeps the PI functional for existing brands while node still ships.
  - fields.rs list/lookup -o/-s field selection ported as-is so `vmadm list -p` and `vmadm lookup -j` output shape matches node.
  - Install rule renames the Rust bin to /usr/vm/sbin/vmadm (replacing the node mv of vmadm.js).
- **verification**:
  - Byte-diff Rust `vmadm list -p` against golden vmadm_list_p.txt (8 VMs, colon-delimited UUID:TYPE:RAM:STATE:ALIAS, trailing blank line). Must match field order and the BHYV-stopped row.
  - Diff `vmadm lookup -j` against golden vmadm_lookup_j.json (8 VM objects, 25032 bytes) with jq -S canonicalization; reconcile key-set differences (note: golden was captured on a newer node base build).
  - Diff `vmadm get 4256eeee-c8b0-4fed-9e0a-975a31e14571` against golden vmadm_get_first.json (49 top-level keys, 2368 bytes).
  - Live create/start/stop/delete of a bhyve and a joyent-minimal zone on .194 or a scratch CN; compare resulting zonecfg against node-produced zonecfg using the vnext tests/zonecfg_golden.rs harness.
  - Confirm `vmadm` on a kvm payload either errors cleanly or dispatches to the legacy node engine per decision.

### P2 — Standalone leaf CLIs: json, bunyan, disklayout, mkzpool, filewait, ntp_config, add-userscript, vmdf(drop)
- **approach**: mixed | **risk**: medium | **effort**: L | **depends on**: [0]
- **components**: src/json.js, src/node-bunyan (bunyan CLI), src/disklayout.js + node_modules/disklayout.js, src/mkzpool.js, src/filewait.js, src/smartdc/lib/ntp_config.js, src/vm/sbin/add-userscript.js, src/vm/sbin/vmdf.js
- **deliverables**:
  - json: rewrite-from-scratch as a clap CLI over serde_json implementing the json 5.1.1 surface scripts actually use (key lookup `json a`, -a array mode, -c condition, jsony/compact output). vnext has NONE; this is the linchpin consumed by sysinfo, piadm, nictagadm, smartos_prompt_config, config.sh, vminfod. Ship as /usr/bin/json.
  - bunyan: rewrite-from-scratch pretty-printer CLI (level mapping 10/20/30/40/50/60 -> trace..fatal, -l/-o/-c flags). vnext has only a one-way parser (log_tailer.rs); reuse its level-mapping logic. Ship as /usr/bin/bunyan + man page.
  - disklayout: rewrite-from-scratch. The vnext disks.rs is inventory-only; the POOL-LAYOUT PLANNER (single/mirror/raidz1-3, -w width, -s spares, -e exclude, -c no-cache) has no Rust. Port the algorithm from src/node_modules/disklayout.js (760 LOC, pure-algorithm) into Rust. Ship as /usr/bin/disklayout.
  - mkzpool: rewrite-from-scratch thin CLI that reads the disklayout JSON and shells out to /usr/sbin/zpool create with -B/-e/-f. Reuse the zfs/zpool shellout wrapper from P3. Ship as /usr/bin/mkzpool.
  - filewait: rewrite-from-scratch using the `notify` crate (FEN on illumos) with a poll fallback; block until path appears, exit 0/2. Ship as /usr/vm/sbin/filewait.
  - ntp_config: rewrite-from-scratch (CIDR->netmask, restrict/pool/server directives, atomic temp+rename). Ship as /smartdc/lib/ntp_config.
  - add-userscript: rewrite-from-scratch (tiny: read file -> emit create/update JSON with customer_metadata user-script). Ship as /usr/vm/sbin/add-userscript.
  - vmdf: DROP. Experimental, unsupported, not jslint-checked, not renamed on install. Remove its install copy and manifest entry.
- **verification**:
  - json: diff `echo '{"a":1}' | json a` => `1` against golden json_sample.txt; `json --version` semantics (decide whether to print 5.1.1 for script compatibility) against golden json_version.txt; run the full sysinfo/nictagadm/config.sh shell scripts end-to-end and diff their JSON-derived output.
  - bunyan: feed a captured bunyan log stream through both node bunyan and Rust bunyan; diff rendered output. `bunyan --version` per golden bunyan_version.txt (1.3.4 string compat decision).
  - disklayout: diff `disklayout -h` usage and several layouts (single/mirror/raidz1-3) against node disklayout output for the same -f disk file; cite golden disklayout_usage.txt for the usage/layout list.
  - mkzpool: dry-run argv comparison vs node mkzpool for the same layout JSON (no destructive run in CI).
  - filewait: test-filewait.js equivalent — create a file after a delay, assert exit 0.
  - ntp_config: diff generated ntp.conf against node ntp_config for identical -s/-a/-t/-o inputs.

### P3 — Native node-zfs / kstat / sysinfo / zoneevent host wrappers (Rust shellout layer)
- **approach**: mixed | **risk**: medium | **effort**: M | **depends on**: [0]
- **components**: src/node_modules/zfs.js, src/node-kstat (kstat.node addon), src/node_modules/system.js, node_modules/net-boot-config.js + src/net-boot-config CLI, src/node_modules/getopt/async/assert-plus/dashdash/linestream/sprintf (drop after consumers gone)
- **deliverables**:
  - zfs/zpool wrapper: copy-from-vnext tritond-cn-platform smartos/zfs.rs (1023 LOC, reproduces node-zfs listFields_ verbatim) into src/rust as a shared lib crate used by mkzpool (P2), imgadm (P8), and the vmadm engine where needed. Replaces /usr/node/node_modules/zfs.js.
  - kstat: copy-from-vnext tritond-cn-platform smartos/kstat.rs (716 LOC). NOTE this shells out to /usr/bin/kstat -p, it does NOT replace the kstat.node addon's API. Provide it as a lib for the vminfod/sysinfo consumers; the kstat.node native addon is deleted only in P10 once system.js's consumers are gone.
  - system.js: has NO in-repo runtime consumer (confirmed in intel). DROP its install + manifest entry. If an external agent needs getProvisionableMemory, expose it via the kstat lib.
  - sysinfo: KEEP the bash /usr/bin/sysinfo script, but replace its node callees (it execs /usr/bin/json and /usr/lib/sdc/net-boot-config). After P2 (json) and the net-boot-config rewrite below, sysinfo no longer transitively needs node.
  - net-boot-config: rewrite-from-scratch (277 LOC pure-JS lib + CLI; flatten nictags/vnics/aggregations to sdc-config key=value). Ship lib logic inside a Rust bin at /usr/lib/sdc/net-boot-config; remove the /usr/node copy.
  - getopt/async/assert-plus/dashdash/linestream/sprintf: these are pure-JS platform modules required only by node consumers; mark for deletion in P10 (they have no standalone install target to rewrite).
- **verification**:
  - sysinfo: after P2+net-boot-config land, run `sysinfo` and `sysinfo -p` and diff against golden sysinfo.json (41 keys, 1728 bytes) and sysinfo_p.txt (49 KEY=VALUE lines). Confirms json+net-boot-config replacements are wire-compatible.
  - zfs wrapper: diff parsed `zfs list -H -p` / `zpool list -Hp` JSON output of the Rust lib against node-zfs for the same datasets.
  - kstat: diff `kstat -p zfs:0:arcstats` parsed values (pages*PAGE_SIZE) against node system.js getMemoryInfo on .102.
  - net-boot-config: diff `/usr/lib/sdc/net-boot-config --enabled` and the flattened key=value output against node on a CN with networking.json.

### P4 — vminfod rewrite (the linchpin daemon) + vminfo CLI + vmload + client
- **approach**: rewrite-from-scratch | **risk**: high | **effort**: XL | **depends on**: [0, 3]
- **components**: src/vm/sbin/vminfod.js, src/vm/node_modules/vminfod/{vminfod,zonewatcher,zpoolwatcher,client}.js, src/vm/sbin/vminfo.js, src/vm/node_modules/vmload/*, src/vm/node_modules/{sysevent-stream,fswatcher,queue,hrtime,diff}.js, src/vm/sbin/fswatcher (native C, retained)
- **deliverables**:
  - Rust vminfod daemon: in-memory authoritative VM cache + HTTP server on 127.0.0.1:9090 (axum or hyper) serving GET /ping, /status, /vms, /vms/:uuid, and streaming GET /events (newline-delimited JSON). vnext has NO vminfod; this is full rewrite. ZoneeventWatcher pattern from vnext cn_status/watchers.rs can seed the zone-event consumption (it shells out to /usr/vm/sbin/zoneevent OR subscribes to sysevent).
  - Event sources: port ZoneWatcher (com.sun:zones:status) and ZpoolWatcher (EC_zfs) — either keep shelling out to /usr/sbin/sysevent (mirror sysevent-stream.js) or use a libsysevent FFI binding (open_question). FsWatcher: KEEP the native C /usr/vm/sbin/fswatcher binary and spawn it `-r -j` (it is non-node C, no eradication needed), parse its JSON.
  - vmload: rewrite-from-scratch the getZoneData assembler (zone XML via expat-free parsing using quick-xml, zoneadm -p, zoneinfo, zfs datasets via P3 lib, config/metadata.json). This replaces the expat native addon's XML role for zone config parsing.
  - vminfod/client: Rust client lib (ping/status/vms/vm + event stream) consumed by the vmadm engine, metadata agent, vmadmd.
  - vminfo CLI: rewrite-from-scratch thin HTTP client (ping/status/vms/vm/events). Ship as /usr/sbin/vminfo. The vminfod SMF start method's `vminfo ping` gate keeps working.
  - SMF: keep system-vminfod.xml but point exec at the Rust binary; update loctext to drop '(node)'.
- **verification**:
  - Diff `vminfo` usage output against golden vminfod_usage.txt (commands: ping, status [-jf], vms, vm <uuid>, events [-jfr]).
  - Stand up Rust vminfod and node vminfod side-by-side on a scratch CN (different ports); diff GET /vms JSON for the same set of zones.
  - Diff `vmadm lookup -j` (which reads vminfod via the client) against golden vmadm_lookup_j.json once the Rust client is wired into the P1 engine.
  - Drive a zone state transition (boot/halt) and assert the Rust /events stream emits the same create/modify/delete records (diff against node /events).
  - Verify the vminfod SMF service comes online (start method `vminfo ping` loop succeeds) under svcadm.

### P5 — metadata agent rewrite (mdata socket protocol server)
- **approach**: rewrite-from-scratch | **risk**: high | **effort**: XL | **depends on**: [0, 4, 1]
- **components**: src/vm/sbin/metadata.js, src/vm/lib/metadata/{agent,common,crc32}.js, VminfodWatcher consumer
- **deliverables**:
  - Rust metadata daemon implementing the SmartOS mdata line protocol (NOT the vnext IMDS HTTP re-architecture — that is AWS-EC2-style and does not serve mdata-get over the zone socket). For OS/LX zones: per-zone unix socket /var/zonecontrol/<zonename>/metadata.sock; for KVM/bhyve: connect as client to <zonepath>/root/tmp/vm.ttyb serial device.
  - Implement GET/KEYS (V1) and V2 GET/PUT/DELETE/NEGOTIATE with CRC32 framing (port crc32.js logic). Handle sdc: namespace (nics/resolvers/routes/tmpfs/tags/operator-script/volumes), customer_metadata, internal_metadata.
  - PUT/DELETE write back through the P1 vmadm engine (VM.update equivalent) so edits ripple to zonecfg + vminfod cache.
  - Consume vminfod via the P4 client (VminfodWatcher equivalent) for zone discovery; reconnect/backoff timers (ECONNREFUSED/ENOENT) and the 60s periodic re-scan (OS-7139).
  - SMF: keep system-metadata.xml dependency on vminfod; point exec at the Rust binary.
- **verification**:
  - In a running OS zone, `mdata-get sdc:nics`, `mdata-get sdc:resolvers`, `mdata-put`, `mdata-delete`, `mdata-list` against the Rust socket; diff responses against the node metadata agent on .102 for an identical VM object.
  - Verify CRC32 V2 framing byte-for-byte by capturing the socket protocol bytes from node vs Rust for the same GET.
  - Confirm mdata-fetch in-zone consumes sdc:nics/resolvers/routes correctly at boot.
  - Verify a metadata PUT updates the VM object (round-trip through vmadm get).

### P6 — vmadmd daemon rewrite (console proxy, QMP/bhyvectl control, autoboot, restart policy)
- **approach**: mixed | **risk**: high | **effort**: XL | **depends on**: [0, 1, 4]
- **components**: src/vm/sbin/vmadmd.js, src/vm/node_modules/{qmp,zonecfg,zoneevent,nic}.js, src/vm/sbin/rotate-kvm-logs.sh (retained, shell)
- **deliverables**:
  - Rust vmadmd daemon: HTTP control API on 127.0.0.1:8080 (POST /vm/:uuid?action=stop|reset|sysrq|reload_display, GET /vm/:uuid/info). The P1 vmadm engine calls this for stop-with-timeout/info/reset/sysrq.
  - Console proxy: copy-from-vnext the console-serving slice (tritonagent console.rs, 1572 LOC) — zoneadmd .console_sock with 'IDENT C 0' handshake (5s timeout, matches this branch's recent commits) for non-KVM, and <zonepath>/root/tmp/vm.console for KVM. Adapt the TLS-WebSocket transport down to the raw TCP-proxy-to-admin_ip model that vmadmd uses today, OR keep vnext's model if console clients can be updated (open_question).
  - QMP client: rewrite-from-scratch (qmp.js is only 197 LOC) for KVM/bhyve stop/reset/sysrq/screenshot. ONLY needed if KVM is kept (see P9 brand decision); for bhyve, drive bhyvectl shellout as vnext does.
  - Zone state watcher: reuse the P4 vminfod /events client (zoneevent->VM.events equivalent) to drive autoboot, provisioning completion, KVM stop-timers, Docker restart policy.
  - upgradeVM/finishUpgrade NIC normalization (nic.js upgradeNics, 197 LOC) ported to Rust.
  - rotate-kvm-logs.sh: KEEP (bash, no node). vmadmd execs it by absolute path.
  - SMF: keep system-vmadmd.xml dependency on vminfod; point exec at Rust binary.
- **verification**:
  - vmadm stop <uuid> -t <timeout> on a bhyve VM exercises the Rust vmadmd HTTP API; compare behavior/timing against node vmadmd.
  - Console: `vmadm console <uuid>` for an OS zone and a bhyve zone; verify the IDENT handshake and interactive I/O against node behavior.
  - Autoboot: reboot the CN (or simulate) and confirm autoboot-flagged VMs come up via the Rust daemon's /events-driven logic.
  - Diff GET /vm/:uuid/info (vnc/spice/screenshot fields) against node vmadmd for a running KVM/bhyve VM.
  - Reset/sysrq round-trip against a bhyve guest.

### P7 — fwadm + firewall engine + ipf rewrite
- **approach**: rewrite-from-scratch | **risk**: high | **effort**: XL | **depends on**: [0, 1]
- **components**: src/fw/sbin/fwadm, src/fw/lib/{fwadm,cli,fw,ipf,filter,rvm,locker,pipeline,util/*}.js, fwrule DSL parser, VM.js->fw.js in-process edge
- **deliverables**:
  - Rust firewall engine (replaces fw.js 3054 LOC + filter/rvm/ipf/locker): fwrule DSL parser (from/to ips/tags/vms/wildcards, owner_uuid-aware, priority/keep-state), per-zone ipf.conf/ipf6.conf generation (inline IPs, NO ippools), and reload via `ipf -G -E -I -Fa -f <conf4> -6 -Fa -f <conf6> -sy <uuid>`. vnext has NONE (only CloudAPI FirewallRule DTOs).
  - State persistence: /var/fw/rules/<uuid>.json, /var/fw/vms/<uuid>.json (RVMs), /var/fw/.lockfile. Replace qlocker+fs-ext native flock with the `fs2`/`rustix` flock crate (acquireShared/Exclusive/release).
  - fwadm CLI (replaces fwadm.js 1236 LOC): full subcommand surface (add/add-rvm/delete/delete-rvm/disable/enable/get/get-rvm/list/list-rvms/update/rules/rvm-rules/start/stop/status/stats/vms), -p/-j/-o output, onlyif.rootInSmartosGlobal GZ-root gate. Ship as /usr/sbin/fwadm (symlink -> /usr/fw/sbin/fwadm).
  - CRITICAL in-process edge: VM.js currently does require('/usr/fw/lib/fw') and calls fw.add/update/enable/disable on provision/update/reboot. The P1 Rust vmadm engine must call the Rust firewall engine as a library (link the fw crate into tritond-vmadm) OR shell out to `fwadm`. Decide and wire (this is the real coupling, not the CLI).
  - fwadm start/stop subcommands route through the vmadm engine (firewall_enabled toggle) — wire to P1 engine.
  - Migrate/preserve existing /var/fw state and per-zone ipf.conf files; bash completion + man pages (leaf) ported or regenerated.
- **verification**:
  - Diff `fwadm list -p` against golden fwadm_list_p.txt (2 default rules icmp6+icmp, colon-delimited UUID:enabled:rule-text).
  - Diff `fwadm help` subcommand list against golden fwadm_help.txt (confirm stats/show absence matches: stats parses as UUID, show is unknown — replicate exactly).
  - Add a rule via Rust fwadm, then diff the generated <zonepath>/config/ipf.conf against node fwadm output for the same rule.
  - Provision a VM with firewall_enabled=true through the P1 engine and confirm the firewall engine is invoked and ipf reloaded (compare ipfstat -hoi -G <uuid> against node path).
  - fwadm status/stats <vm-uuid> output diffed against node.

### P8 — imgadm rewrite (image install via direct zfs + IMGAPI source) + IMG.js fast path
- **approach**: mixed | **risk**: high | **effort**: XL | **depends on**: [0, 3, 1]
- **components**: src/img/sbin/imgadm + lib/{cli,imgadm,common,IMG,database,configuration,upgrade,magic}.js, lib/sources/{index,source,imgapi,dsapi,docker}.js, chroot-gtar/gtar-unlink-dir/nomknod.so (docker only), VM.js->imgadm get edge
- **deliverables**:
  - Rust imgadm CLI + engine: sources (list/add/delete), avail, show, import, install, list (--docker), get/info, update, delete, ancestry, vacuum, create, publish. Partial reuse from vnext: tritonagent images.rs (496 LOC direct-zfs import: GET stream -> sha verify -> zfs receive -> @final), imgapi-manifest (521 LOC, full IMGAPI v2 manifest model), tritond-image-manifest, imgapi-blob-manta (S3 transport). HTTP via reqwest+rustls.
  - ZFS coupling via the P3 zfs lib (shell to /usr/sbin/zfs, /usr/sbin/zoneadm). DB at /var/imgadm/images/<zpool>-<uuid>.json + imgadm.conf; legacy /var/db/imgadm upgrader (upgradeTo200/300).
  - IMG.js fast path: rewrite-from-scratch quickGetImage (zfs-free DB read) for vminfod/vmadm — wire it into the P1 engine if the engine should avoid spawning imgadm.
  - CRITICAL edge: VM.js spawns `/usr/sbin/imgadm get -P <zpool> <uuid>` at lines 823 + 11430 for image validation and image_size. The P1 Rust vmadm engine must either call the Rust imgadm `get` or use the IMG.js-equivalent lib in-process. Wire this so VM image validation/resize keeps working.
  - Docker source + chroot-gtar + gtar-unlink-dir + nomknod.so: DROP if docker brand is dropped in P9 (they become dead together). Otherwise port docker.js source + keep the C nomknod.so (it is not node).
  - dsapi source: DROP (doubly deprecated). dsadm symlink: DROP.
  - Ship /usr/sbin/imgadm -> /usr/img/sbin/imgadm.
- **verification**:
  - Diff `imgadm list -j` against golden imgadm_list_j.json (13 image manifest entries, 14114 bytes) with jq -S.
  - Diff `imgadm sources` against golden imgadm_sources.txt (single source https://images.smartos.org).
  - Import a known image from images.smartos.org on a scratch CN; compare the resulting zones/<uuid>@final dataset and /var/imgadm manifest against the node path.
  - Confirm the P1 vmadm engine's image validation (the imgadm get edge) succeeds for an installed image and fails cleanly for a missing one (node maps 404 behavior).
  - imgadm create on a joyent-minimal VM (operator-script prepare-image contract) end-to-end.

### P9 — KVM/lx/docker brand decision execution (the brand-scope reduction)
- **approach**: mixed | **risk**: high | **effort**: XL | **depends on**: [1, 5, 6, 8]
- **components**: tritond-vmadm brand.rs, node vmadm-legacy fallback, KVM-specific code in vmadmd/metadata/imgadm, docker source + nomknod + chroot-gtar
- **deliverables**:
  - Execute the brand-scope decision (BLOCKER — see open_questions). Three viable paths the plan supports: (A) DROP kvm/lx/docker entirely from the PI: remove their brand handling, docker image source, nomknod.so, chroot-gtar; the Rust vmadm errors UnsupportedBrand for them — fully removes node. (B) REWRITE kvm/lx in Rust: extend tritond-vmadm brand.rs with KVM (QMP from P6) and lx zonecfg generation — large net-new work, no vnext reuse. (C) KEEP node ONLY for kvm/lx behind the P1 dispatch shim, accepting that /usr/node survives for those brands — this CONTRADICTS the full-eradication goal and is the fallback if (A) is unacceptable and (B) is too costly.
  - If (A): delete the docker eradication leftovers (lib/sources/docker.js, chroot-gtar, gtar-unlink-dir, nomknod.so build + manifest), remove KVM QMP code paths, document the platform no longer hosts those brands.
  - If (B): add KVM/lx brand modules to tritond-vmadm, KVM QMP lifecycle to vmadmd (P6), KVM serial mdata path to metadata (P5 already covers the bhyve/KVM serial client), and KVM image handling to imgadm (P8).
  - Migration/audit: scan all CNs for existing kvm/lx/docker zones before this phase; the decision must not strand running guests.
- **verification**:
  - If (A): confirm no kvm/lx/docker zones exist on target CNs; `vmadm create` with a kvm payload returns a clear UnsupportedBrand error; PI has no /usr/img docker helpers.
  - If (B): create/start/stop/console a KVM and an lx zone with the Rust engine; diff zonecfg + lifecycle against node.
  - If (C): confirm the dispatch shim routes kvm/lx to the retained node engine and that node 0.10 + only the kvm-required addons remain (documented deviation from full eradication).

### P10 — Delete node runtime, native addons, vendored modules, and all node build wiring
- **approach**: drop | **risk**: high | **effort**: L | **depends on**: [1, 2, 3, 4, 5, 6, 7, 8, 9]
- **components**: /usr/node tree (sdcnode), 7 native addons (kstat/zonename/libuuid/expat/dtrace-provider/qlocker/fs-ext), src/node-* dirs, src/node_modules/*.js, Makefile.defs node vars, src/Makefile node rules, src/manifest node entries, tools/jr-manifest.json, configure nodejs prereq, man/Makefile ronnjs
- **deliverables**:
  - src/Makefile.defs: delete PREFIX_NODE (L65), NODE_EXEC (L66, dead), NPM_EXEC (L67), NODE_WARN_FLAGS, NODE_CFLAGS/NODE_CXXFLAGS, NODE_DEFS, NODE_INCS, NODE_CPPFLAGS, NODE_LIBDIR, NODE_LDFLAGS.
  - src/Makefile: delete the addon block (NODE_DESTDIR + EXPAT/UUID/KSTAT vars+flag rules L178-215, NODE_SUBDIR_ENV + the 3 npm-install rules + CLEANFILES L217-258), the link rules (L593-603), the props.js expander rule (L585-587), remove $(EXPAT.NODE)/$(KSTAT.NODE)/$(UUID.NODE)/props.js/json from BUILT_TARGETS and json/disklayout/mkzpool from USRBIN_TARGETS (now Rust-built), drop NPM_TARGETS, and gut the /usr/node staging + bunyan shebang sed + dtrace require-rewrite + /usr/fw dtrace/bunyan copy in the install target (L420-506).
  - src/manifest: delete all 40 usr/node/* entries plus the d usr/node dirs; delete usr/fw/node_modules/{bunyan.js,dtrace-provider.js,DTraceProviderBindings.node,onlyif.js} entries; replace the node tool entries (usr/vm/sbin/*, usr/img/sbin/imgadm, usr/fw/sbin/fwadm, usr/bin/{json,bunyan,disklayout,mkzpool}, usr/vm/sbin/{filewait,zoneevent}) with the Rust-installed binaries (same paths/perms).
  - Delete src/node-bunyan, src/node-dtrace-provider, src/node-expat, src/node-kstat, src/node-libuuid, src/node-qlocker, src/node-zonename directories and src/node_modules/*.js (async, assert-plus, dashdash, getopt, linestream, sprintf, onlyif, system, zfs, disklayout, net-boot-config).
  - Delete src/vm/node_modules/* JS engine libs (VM.js, vmload, vminfod/*, fw client refs, props/proptable/expander, qmp, zonecfg, zoneevent, nic, dladm, ip, utils, diff, fswatcher.js, sysevent-stream, queue, hrtime, openonerrlogger, nopt) and src/fw/lib/* + src/fw/node_modules + src/img/lib/* + src/img/node_modules now that Rust replaces them.
  - tools/jr-manifest.json: drop node-dtrace-provider, node-fs-ext, node-qlocker, libusdt entries.
  - configure: remove `nodejs` from the build prerequisite pkglist (L205). Coordinate the actual /usr/node binary removal with projects/illumos-extra (the node binary is supplied there, NOT built in smartos-live).
  - man/Makefile: replace ronnjs (node) with a non-node md->roff tool (go-md2man or pandoc) OR keep ronnjs on a host node — decide. javascriptlint (python) and jsstyle (perl) are unaffected; remove the now-dead JS_CHECK_TARGETS.
  - Coordinate with illumos-extra/sdcnode to stop laying down /usr/node/0.10/bin/node so the final PI has no /usr/node.
- **verification**:
  - Build a full PI on .194; assert `find proto/usr/node` is empty/absent and `find proto -name '*.node'` returns nothing.
  - Boot the PI (or the .102-equivalent test CN); confirm svc:/system/smartdc/{vminfod,vmadmd,metadata} come online with the Rust daemons.
  - Re-run the ENTIRE golden suite (vmadm list -p, vmadm lookup -j, vmadm get, imgadm list -j, imgadm sources, fwadm list -p, fwadm help, sysinfo, sysinfo -p, nictagadm list -p, json sample, disklayout usage, vminfo usage) and diff against the .102 captures.
  - Grep the booted system for `/usr/node/bin/node` references in any shebang or running process (`pgrep node` returns nothing; no SMF method references node).
  - Confirm `ls /usr/node` does not exist and the symlink /usr/node/node_modules -> ./0.10/node_modules is gone (was confirmed present on .102 via node_modules_ls.txt).


## Workspace design

Location: a single cargo workspace at src/rust/ inside smartos-live (the repo is the self-contained home; no external triton-vnext dependency at build time). The vnext crates are COPIED in, not git-referenced.

Workspace root src/rust/Cargo.toml reproduces mariana-trench's settings verbatim so lifted crates compile unchanged: resolver=2; [workspace.package] edition=2024; [profile.dev] and [profile.release] panic='abort'; [workspace.lints.clippy] unwrap_used='deny', expect_used='deny'. A local [workspace.dependencies] table carries the concrete versions for the engine-path closure only (the central table captured from mariana-trench): anyhow 1.0, async-trait 0.1, base64 0.22, chrono 0.4(serde), clap 4.5(derive,env), ipnetwork (serde,no-default), libc, serde 1.0(derive), serde_json 1.0, thiserror 2.0, tokio 1.48(full), tracing 0.1, uuid 1(v4,v5,serde), tempfile 3; plus the later-phase additions reqwest+rustls+webpki-roots+sha2+flate2 (imgadm P8), quick-xml (vmload XML, P4), notify (filewait/vminfod, P2/P4), fs2 or rustix (fwadm flock, P7), axum/hyper (vminfod/vmadmd HTTP, P4/P6), tokio-tungstenite+axum-server+rustls (console, P6). NO dropshot/progenitor/proteus/opte/foundationdb — those are tritonagent-only and must NOT be pulled in.

Members grow per phase: P0 just a hello bin; P1 adds tritond-vmadm + tritond-cn-platform + cli/vmadm; P2 adds bins json/bunyan/disklayout/mkzpool/filewait/ntp_config/add-userscript/net-boot-config; P3 a shared smartos-host lib (zfs/kstat/sysinfo wrappers); P4 vminfod+vminfo+vmload+vminfod-client; P5 metadata; P6 vmadmd; P7 fwadm engine+cli; P8 imgadm engine+cli + imgapi-manifest/image-manifest libs.

Vendoring for a hermetic illumos build: smartos-live ships with no network access at build time, so all crates.io deps are vendored. Run `cargo vendor src/rust/vendor` and check the vendor/ tree into the repo. src/rust/.cargo/config.toml sets [source.crates-io] replace-with='vendored-sources' and [source.vendored-sources] directory='vendor', plus build.target='x86_64-unknown-illumos'. Cargo.lock is committed. Builds run `cargo build --release --offline --locked --frozen`. A rust-toolchain.toml pins the stable channel + the x86_64-unknown-illumos target so the build host compiler is reproducible. The proteus-workspace-collision hazard from vnext (the `exclude=['rust']` + CARGO_HOME redirect) does NOT apply here because we are not vendoring opte/proteus.

Target triple: x86_64-unknown-illumos is a Tier-2 target present in stable rustc (verified: `rustc --print target-list` lists it). The .102 platform is joyent_20260622T191148Z x86_64; the Rust binaries link against the platform libc/illumos shared libs already on the system. No -m32 (unlike node addons): these are native 64-bit illumos ELF binaries, eliminating the 32-bit ABI coupling entirely.

How gmake invokes cargo: src/Makefile.defs adds CARGO=<path-to-cargo>, CARGO_TARGET=x86_64-unknown-illumos, RUST_RELEASE_DIR=src/rust/target/$(CARGO_TARGET)/release. A new src/rust/Makefile (gmake) exposes a phony `all` that runs `$(CARGO) build --release --offline --locked` and a `rust_check` that runs `$(CARGO) clippy --offline -- -D warnings` + `$(CARGO) test --offline`. src/Makefile gains src/rust as a SUBDIR (built before the install target) and the install target copies each Rust binary from RUST_RELEASE_DIR to its final path (e.g. cp $(RUST_RELEASE_DIR)/vmadm $(DESTDIR)/usr/vm/sbin/vmadm) instead of the old node mv .js rules. The cargo toolchain itself is provisioned the same way node was: into proto.strap by the configure/illumos-extra adjunct layer (decision in open_questions — pkgsrc rust vs rustup target add).

Why this is clean: tritond-vmadm depends on exactly ONE workspace crate (tritond-cn-platform), which depends on ZERO workspace crates — a shallow, leaf-only dependency tree. The whole OS-coupling surface is std::process shellout to illumos binaries (zonecfg/zoneadm/zfs/zpool/svccfg/nictagadm/lofiadm/ipf/sysevent/kstat) plus one libc flock; all of it is behind traits (ZoneHost/NicTagRegistrar) so the code compiles on macOS for unit tests and only the live behavior is illumos-specific. No build.rs, no FFI codegen, no generated OpenAPI clients in the engine path.

## Build-system changes

1. src/Makefile.defs: ADD (P0) CARGO, CARGO_TARGET=x86_64-unknown-illumos, RUST_RELEASE_DIR, CARGO_FLAGS='--offline --locked --frozen'. DELETE (P10) PREFIX_NODE (L65), NODE_EXEC (L66 dead), NPM_EXEC (L67), NODE_WARN_FLAGS (L84), NODE_CFLAGS/NODE_CXXFLAGS (L88-89), NODE_DEFS (L92), NODE_INCS (L95), NODE_CPPFLAGS (L97), NODE_LIBDIR (L101), NODE_LDFLAGS (L103).
2. src/Makefile: ADD (P0) src/rust to SUBDIRS so `gmake -C src/rust` runs before install; ADD a rust_check hook into the check target.
3. src/Makefile: install target — REPLACE node mv rules incrementally. P1: replace `mv .../vmadm.js .../vmadm` (L465) with `cp $(RUST_RELEASE_DIR)/vmadm $(DESTDIR)/usr/vm/sbin/vmadm`. P2: replace json/disklayout/mkzpool USRBIN cp + filewait/add-userscript cp with Rust-binary cp; remove vmdf install copy entirely. P4-P8: replace vminfod/vminfo/metadata/vmadmd mv rules (L464-467) and imgadm/fwadm tree copies with Rust-binary cp.
4. src/Makefile (P10): DELETE NODE_DESTDIR + EXPAT/UUID/KSTAT object+flag rules (L178-215); DELETE NODE_SUBDIR_ENV + the 3 npm-install rules (dtrace-provider/qlocker/zonename) + their CLEANFILES (L217-258); DELETE the addon link rules (L593-603); DELETE the props.js expander rule (L585-587); REMOVE $(EXPAT.NODE)/$(KSTAT.NODE)/$(UUID.NODE)/vm/node_modules/props.js from BUILT_TARGETS (L294,300-303) and json/disklayout/mkzpool from USRBIN_TARGETS (L312-318); DELETE NPM_TARGETS (L305-310) and drop from TARGETS (L323-325); GUT the /usr/node staging block, bunyan shebang sed + man, dtrace require-rewrite, node-expat.js sed, and the /usr/fw bunyan+dtrace copies (L420-506).
5. src/manifest (P1-P8): each phase, KEEP the final path entries (f usr/vm/sbin/vmadm 0555, f usr/img/sbin/imgadm 0555, f usr/fw/sbin/fwadm 0555, f usr/bin/json 0555, f usr/bin/disklayout 0555, f usr/bin/mkzpool 0555, f usr/vm/sbin/filewait 0555, etc.) — they now point at Rust binaries, same perms. DELETE the vmdf entry. P2: add f usr/bin/bunyan 0555.
6. src/manifest (P10): DELETE all 40 usr/node/* lines (L463-475 + L1129-1147 incl. d usr/node, d usr/node/0.10, node_modules tree, *.node addons, qlocker/, fs-ext/, src/build/Release/, all *.js platform modules); DELETE usr/fw/node_modules/{bunyan.js,dtrace-provider.js,DTraceProviderBindings.node,onlyif.js,dashdash.js,...} entries; DELETE usr/img/lib/{nomknod.so,amd64/nomknod.so,64 symlink} + chroot-gtar/gtar-unlink-dir IF docker dropped (P9); DELETE usr/sbin/dsadm symlink; DELETE legacy d var/db/imgadm.
7. tools/jr-manifest.json (P10): DELETE node-dtrace-provider (L93-95), node-fs-ext (L96-98), node-qlocker (L99-101), libusdt (L90-92).
8. configure (P10): REMOVE `nodejs` from the build-prereq pkglist (L205). ADD (P0) the Rust toolchain to the same provisioning path (pkgsrc rust + x86_64-unknown-illumos target, OR rustup). Coordinate /usr/node binary removal with projects/illumos-extra (the node 0.10 binary comes from sdcnode/illumos-extra, NOT smartos-live).
9. man/Makefile (P10): REPLACE RONNJS=/opt/local/bin/node ... ronn.js (L7) with a non-node md->roff renderer (go-md2man or pandoc), OR retain ronnjs on a host node since it does not depend on the platform node. Remove dead JS man sources only after the JS tools are deleted.
10. src/rust/ NEW FILES (P0): Cargo.toml (workspace), Cargo.lock, .cargo/config.toml, rust-toolchain.toml, vendor/ (checked-in cargo vendor output), Makefile (gmake->cargo wrapper).

## Verification strategy

Two-axis verification: (1) byte-exact golden diffs against the live node .102 captures, and (2) live build+boot on the .194 host once it returns.

Golden-diff harness: the .102 captures in scratchpad/golden/ are the oracle. For each replaced tool, run the Rust binary on an equivalent CN and diff: vmadm list -p vs vmadm_list_p.txt (exact, incl. trailing blank line, colon-delimited 8-VM table); vmadm lookup -j vs vmadm_lookup_j.json and vmadm get <uuid> vs vmadm_get_first.json (jq -S canonicalized, reconciling top-level key sets — 49 keys for get, 8 objects for lookup); imgadm list -j vs imgadm_list_j.json (13 entries) and imgadm sources vs imgadm_sources.txt; fwadm list -p vs fwadm_list_p.txt (2 default rules) and fwadm help vs fwadm_help.txt (replicate that stats parses as a UUID and show is 'unknown command'); sysinfo vs sysinfo.json (41 keys) and sysinfo -p vs sysinfo_p.txt (49 lines) — this transitively validates the json + net-boot-config rewrites; nictagadm list -p vs nictagadm_list_p.txt (validates json consumed by the bash tool); echo '{\"a\":1}' | json a vs json_sample.txt; disklayout -h vs disklayout_usage.txt; vminfo usage vs vminfod_usage.txt.

CRITICAL CAVEAT baked into the strategy: the shipped VM.js on .102 (md5 f1e9b8d0...) is a NEWER base build than this checkout AND LACKS the proteus-nic-tag change (grep proteus on shipped VM.js = empty), while imgadm and fwadm match the repo bit-for-bit. Therefore VM.js/vmadm behavioral diffs must be taken against a freshly built node PI from THIS branch (not blindly against .102), or the proteus nic_tag acceptance path must be tested separately. imgadm and fwadm golden diffs CAN be trusted directly against .102.

Live build+boot (gated on .194 returning): build a full PI, assert no /usr/node and no *.node files in proto, boot a test CN, confirm svc:/system/smartdc/{vminfod,vmadmd,metadata} reach online with the Rust daemons (the vminfod start method's `vminfo ping` loop is the gate), then re-run the entire golden suite on the booted system and confirm `pgrep node` is empty and no SMF method references /usr/node/bin/node.

Per-phase parity: P1 uses vnext's tests/zonecfg_golden.rs + FakeHost create/rollback unit tests for engine correctness, and a live zonecfg diff (Rust-produced vs node-produced) for a bhyve and joyent-minimal create. The vnext differential harness (cli/vmadm-difftest, feature=difftest) is referenced in DESIGN.md but ABSENT from the tree — building it (run the same payload through node VM.js and the Rust engine, diff zonecfg/zoneadm argv) is a recommended P1 deliverable to make parity machine-checkable. P4/P5/P6 run Rust and node daemons side-by-side on different ports and diff HTTP/socket responses. Unit tests run on macOS (the FakeHost/trait seams make the code host-agnostic); only live behavior needs illumos.

## Blockers

- BUILD HOST .194 (smartos-build1) is UNREACHABLE (ssh timeout from both the operator and .102). NOTHING can be compiled for illumos until it returns or an alternative illumos build host is provided. macOS cannot build illumos binaries. This blocks ALL compile-level verification of every phase; only unit tests (FakeHost path) run off-host.
- BRAND SCOPE DECISION (gates P9 and the very definition of 'done'): vnext tritond-vmadm supports ONLY bhyve + joyent-minimal; kvm/lx/docker are a hard UnsupportedBrand error. The shipped PI may host those brands. Until the user decides DROP vs REWRITE vs KEEP-NODE-FOR-KVM, full node eradication (P10) cannot complete — option KEEP-NODE-FOR-KVM directly contradicts the stated goal of NO /usr/node.
- RUST TOOLCHAIN PROVISIONING for the illumos build host is not yet wired. configure currently installs `nodejs` as a build prereq; there is no cargo/rustc and no x86_64-unknown-illumos target in the build adjunct. This must be added (pkgsrc rust or rustup) before P0 can build on .194, and coordinated with projects/illumos-extra.
- The vnext DIFFERENTIAL TEST HARNESS (cli/vmadm-difftest, feature=difftest) referenced in DESIGN.md does NOT exist in the tree. Without it, vmadm-vs-node parity is asserted only by golden unit tests, not a live node-vs-native diff — building it is required for a defensible P1 acceptance gate.
- node 0.10 BINARY removal is OUT OF smartos-live's repo: /usr/node/0.10/bin/node is supplied by projects/illumos-extra (sdcnode). P10 can delete all node_modules + addons + Makefile wiring, but the actual interpreter deletion requires a coordinated change in illumos-extra that the user/team must own.

## Open questions (decisions)

- Brand scope: DROP kvm/lx/docker from the PI, REWRITE them in Rust (large net-new, no vnext reuse), or KEEP node solely for kvm/lx behind a dispatch shim (contradicts full eradication)? This is the single biggest scoping decision and changes the size of P9 and whether P10 truly removes /usr/node.
- Rust toolchain delivery to the build host: add `rust` to the pkgsrc/illumos-extra adjunct in proto.strap, or document an explicit `rustup target add x86_64-unknown-illumos` operator step? Affects configure + the hermetic-build guarantee.
- Vendoring policy: commit the full src/rust/vendor/ tree into smartos-live (largest, fully hermetic, matches 'self-contained repo'), or fetch+cache at build time (smaller repo, needs network at build, violates hermetic)? The plan assumes committed vendor/.
- Console transport model (P6): vnext console.rs serves a TLS-WebSocket; node vmadmd advertises raw VNC/SPICE/serial TCP ports on admin_ip. Keep the existing raw-TCP-to-admin_ip model (node-compatible, no client changes) or adopt vnext's WS model (requires updating console clients)?
- metadata datasource: confirm we must preserve the SmartOS mdata line protocol (mdata-get over /var/zonecontrol/<zone>/metadata.sock) for in-zone compatibility, rather than adopting vnext's AWS-IMDSv2 HTTP re-architecture. The plan assumes mdata-protocol preservation; switching would break in-zone mdata-get.
- vminfod event source: keep shelling out to /usr/sbin/sysevent (mirror sysevent-stream.js, lowest risk) or invest in a libsysevent FFI binding for native event subscription? Affects P4 risk/effort.
- VM.js<->fw.js and VM.js<->imgadm in-process edges: should the Rust vmadm engine LINK the Rust firewall and imgadm engines as libraries (faster, tighter coupling) or SHELL OUT to the Rust fwadm/imgadm binaries (looser, mirrors today's process boundaries)? Affects P7/P8 wiring.
- json/bunyan --version strings: many shell scripts may grep for 'json 5.1.1' / 'bunyan 1.3.4'. Should the Rust reimplementations print those legacy version strings for compatibility, or their own versions? Cite golden json_version.txt / bunyan_version.txt.
- man-page build: replace ronnjs with go-md2man/pandoc, or keep ronnjs running on a host node (build-time only, not platform node)? The full-eradication goal arguably includes the build host, which would favor replacing ronnjs.
- Should tritond-cn-platform be lifted whole, or should the single ZoneadmTool call be inlined into tritond-vmadm to drop the dependency entirely (vnext workspace_notes suggests this is feasible and yields a single-crate engine)?

## Appendix A — Node.js inventory (smartos-live)

| component | kind | LOC | install path | node coupling |
|---|---|---|---|---|
| vmadm | cli | 1465 | src/Makefile install target: `cp -PR vm $(DESTDI | Shebang `#!/usr/node/bin/node --abort_on_uncau |
| vmadmd | daemon | 2877 | src/Makefile: copied as part of `cp -PR vm`, the | Shebang `#!/usr/node/bin/node --abort_on_uncau |
| vminfod | daemon | 3018 | src/Makefile: `cp -PR vm` then `mv $(DESTDIR)/us | Shebang `#!/usr/node/bin/node --abort_on_uncau |
| metadata agent | daemon | 1630 | src/Makefile: `cp -PR vm` copies lib/metadata/*. | sbin shebang `#!/usr/node/bin/node --abort_on_ |
| vminfo | cli | 402 | src/Makefile: `cp -PR vm` then `mv $(DESTDIR)/us | Shebang `#!/usr/node/bin/node` (no abort flag) |
| add-userscript | cli | 75 | src/Makefile: `cp -PR vm` then `mv $(DESTDIR)/us | Shebang `#!/usr/node/bin/node` (no abort flag) |
| vmdf | cli | 220 | Copied to /usr/vm/sbin/vmdf.js via the `cp -PR v | Shebang `#!/usr/node/bin/node`. Requires /usr/ |
| VM.js | library | 19671 | src/Makefile: copied via `cp -PR vm` to /usr/vm/ | `require('/usr/node/node_modules/platform_node |
| vmload | library | 3991 | src/Makefile: `vm/node_modules/vmload/*.js` list | Shebangs `#!/usr/node/bin/node` on index/dump  |
| proptable / expander / props.js | build-tool | 2806 | src/Makefile rule (line ~585): `vm/node_modules/ | expander.js runs under the BUILD host `node` ( |
| vminfod/client | library | 1135 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/assert-plus, u |
| qmp | library | 197 | Installed via `cp -PR vm` to /usr/vm/node_module | `require('/usr/node/node_modules/platform_node |
| zonecfg | library | 163 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/{assert-plus,q |
| zoneevent | library | 182 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/assert-plus an |
| nic | library | 197 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/async and /usr |
| dladm | library | 198 | Installed via `cp -PR vm` to /usr/vm/node_module | `platform_node_version.assert()` coupling. Spa |
| ip | library | 280 | Installed via `cp -PR vm` to /usr/vm/node_module | Pure JS, no requires of platform modules or ex |
| utils | library | 358 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/vm/node_modules/{ip,macaddr} and |
| diff | library | 505 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/assert-plus. P |
| fswatcher | library | 943 | Library installed via `cp -PR vm` to /usr/vm/nod | Requires /usr/node/node_modules/{assert-plus,l |
| sysevent-stream | library | 350 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/{assert-plus,l |
| queue | library | 589 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/uuid and /usr/ |
| hrtime | library | 246 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/assert-plus. P |
| openonerrlogger | library | 264 | Installed via `cp -PR vm` to /usr/vm/node_module | Requires /usr/node/node_modules/bunyan. Pure J |
| nopt | library | 0 | Installed via `cp -PR vm` to /usr/vm/node_module | Third-party vendored module; pure JS. No nativ |
| rotate-kvm-logs.sh / rotate-logs.sh | script | 0 | Shipped via `cp -PR vm` to /usr/vm/sbin/rotate-k | Bash scripts, not node; spawn zfs/zpool/logadm |
| man pages (vmadm.8 / vmadmd.8) + completion | script | 0 | man/ and etc/ are copied wholesale via `cp -PR v | None (docs/data files). |
| fwadm CLI (sbin/fwadm + lib/fwadm.js + lib/cli.js) | cli | 1236 | No compile step. src/Makefile install rule: cp - | Hardcoded shebang /usr/node/bin/node (platform |
| fw.js firewall engine | library | 3054 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | npm deps: fwrule (rule parse/serialize/version |
| ipf control wrapper (lib/ipf.js) | library | 311 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | Uses child_process.execFile (no npm CLI-spawn  |
| rule/VM filtering (lib/filter.js) | library | 375 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | npm dep assert-plus; internal mod_obj, mod_rvm |
| remote VM state (lib/rvm.js) | library | 346 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | npm deps: fwrule (generateVersion), mkdirp, va |
| file locking (lib/locker.js) | library | 115 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | NATIVE ADDON DEPENDENCY: requires /usr/node/no |
| pipeline helper (lib/pipeline.js) | library | 68 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | npm dep vasync only. No native/version-specifi |
| logging (lib/util/log.js) | library | 490 | cp -PR fw/lib -> /usr/fw/lib (includes lib/util) | Lazily requires bunyan. On the platform, bunya |
| shared utils (lib/util/obj.js, lib/util/vm.js, lib/util/errors.js) | library | 384 | cp -PR fw/lib -> /usr/fw/lib. Manifest: 'f usr/f | vm.js depends on npm ip6addr + verror; errors. |
| bash completion (etc/fwadm.completion) | script | 58 | cp -PR fw/etc -> /usr/fw/etc. Manifest: 'f usr/f | None (pure bash; depends on zoneadm and the /v |
| ramsey doc/man templating tool (tools/ramsey) | build-tool | 105 | Not installed to the platform. Vendored under sr | Runs under #!/usr/bin/env node at build time.  |
| rebuild-node-modules (tools/bin/rebuild-node-modules) | build-tool | 214 | Not installed. Output (node_modules/) is install | Drives npm; produces the node-0.10-targeted mo |
| imgadm CLI | cli | 2211 | src/Makefile `install` target (lines ~480-488):  | Requires the platform node at /usr/node/bin/no |
| IMGADM engine library | library | 5178 | Installed as part of the /usr/img tree (src/Make | Requires platform-node singletons by absolute  |
| IMG.js fast-path library | library | 81 | src/manifest: `f usr/img/lib/IMG.js 0444 root bi | Only requires assert-plus, ./database, ./error |
| imgadm source adapters | library | 957 | src/manifest installs each under /usr/img/lib/so | Pure JS. Deps: sdc-clients (imgapi, dsapi), do |
| imgadm DB and config layer | library | 655 | src/manifest installs database.js/configuration. | Pure JS: assert-plus, mkdirp, fs, path. No nat |
| chroot-gtar / gtar-unlink-dir extraction helpers | script | 284 | Shipped via `cp -PR img/sbin` in src/Makefile; s | None (POSIX sh/bash). Depends at runtime on il |
| recover-vm-from-failed-imgadm-create.sh | script | 173 | Shipped via `cp -PR img/sbin`; installed under / | None (bash). Shells out to vmadm, zfs, and the |
| nomknod.so shim | native-addon | 0 | Built by src/Makefile, NOT src/img/Makefile: NOM | None — it is a C LD_PRELOAD lib, unrelated to  |
| json | cli | 1326 | Makefile.targ pattern '%: %.js' (cp + chmod 0755 | Shebang #!/usr/node/bin/node. Pure stdlib only |
| disklayout | cli | 137 | Makefile.targ '%: %.js' builds disklayout from d | Shebang #!/usr/node/bin/node. Requires platfor |
| disklayout (library) | library | 760 | Copied as-is into the platform node_modules tree | Plain CommonJS module, only requires('assert') |
| mkzpool | cli | 66 | Makefile.targ '%: %.js' builds mkzpool from mkzp | Shebang #!/usr/node/bin/node. Requires /usr/no |
| filewait | cli | 87 | Plain copy, not the pattern rule: 'cp filewait.j | Shebang #!/usr/node/bin/node. Stdlib only (fs, |
| zoneevent | cli | 155 | Plain copy: 'cp zoneevent.js $(DESTDIR)/usr/vm/s | Shebang #!/usr/node/bin/node. Requires platfor |
| net-boot-config (library) | library | 277 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS module; stdlib only (util.format, fs) |
| system (library) | library | 109 | Copied into platform node_modules: 'cp -Pr node_ | Strongly platform-coupled: first line calls re |
| onlyif (library) | library | 64 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; requires stdlib os plus /usr/node/no |
| sprintf (library) | library | 202 | Copied into platform node_modules: 'cp -Pr node_ | Vendored upstream; first line calls require('/ |
| getopt (library) | library | 304 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; only requires('assert').ok from stdl |
| async (library) | library | 1043 | Copied into platform node_modules: 'cp -Pr node_ | Vendored upstream UMD module; uses setImmediat |
| assert-plus (library) | library | 211 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; requires stdlib assert/stream/util.  |
| dashdash (library) | library | 773 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; requires('assert-plus') (the sibling |
| linestream (library) | library | 64 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; requires stdlib stream (falls back t |
| zfs (library) | library | 610 | Copied into platform node_modules: 'cp -Pr node_ | CommonJS; requires stdlib child_process (execF |
| ntp_config | cli | 311 | Makefile.targ '%: %.js' builds smartdc/lib/ntp_c | Shebang #!/usr/node/bin/node. Requires /usr/no |
| sysinfo | script | 1242 | Plain file copied verbatim: listed in USRBIN_TAR | None directly -- it is shell. BUT it shells ou |
| bunyan | cli | 0 | CLI: 'sed -e "s|^#!/usr/bin/env node.*$|#!/usr/n | Shebang rewritten at install to #!/usr/node/bi |
| node-kstat | native-addon | 337 | Built by inline pattern in src/Makefile (NOT its | Old-style raw V8 API for node 0.10: Handle<Val |
| node-zonename | native-addon | 88 | NPM_TARGET in src/Makefile: rule cd node-zonenam | NAN-based (modern abstraction): #include <nan. |
| node-libuuid | native-addon | 49 | Built by inline pattern in src/Makefile like EXP | Old raw V8 API for node 0.10: Handle<Object>,  |
| node-expat | native-addon | 372 | Built by inline EXPAT pattern (the canonical tem | Old raw V8 API for node 0.10: Persistent<Strin |
| node-dtrace-provider | native-addon | 472 | NPM_TARGET in src/Makefile with NODE_DTRACE_PROV | NAN-based: nan.h, NAN_METHOD, Nan::ObjectWrap, |
| node-qlocker (+ fs-ext) | native-addon | 190 | NPM_TARGET in src/Makefile: cd node-qlocker && n | qlocker itself is pure JS (path/fs/assert + re |
| node-bunyan | cli | 3068 | Pure-JS, no compile. Installed by sed-rewrites i | Pure JS, node>=0.8 per package.json. Soft/opti |
| node-runtime (sdcnode /usr/node 0.10.26) | runtime | 0 | The node BINARY/lib/include is NOT built by smar | Defines the coupling for everything else: node |
| src/Makefile.defs (node build variables) | build-tool | 129 | No install rules of its own; included by src/Mak | Hard-codes node version 0.10 in PREFIX_NODE/NO |
| src/Makefile (node-addon build rules) | build-tool | 611 | Vars: NODE_DESTDIR = $(PREFIX_NODE)/node_modules | C++ addons compiled with N-API-free node 0.10  |
| src/Makefile (npm-install addon rules: dtrace-provider, qlocker, zonename) | build-tool | 611 | NODE_SUBDIR_ENV (L217-226) sets V=1, NODE_DTRACE | Calls NPM_EXEC (node 0.10 npm). Requires pytho |
| src/Makefile (install target: /usr/node, shebang rewrite, bunyan, .js tools) | build-tool | 611 | L420-447 /usr/node/node_modules: mkdir $(NODE_DE | The ONLY shebang rewrite in the Makefile is th |
| vm/node_modules/props.js generator (expander) | build-tool | 611 | Rule L585-587: `node vm/node_modules/expander.js | Build-time dependency on a host `node` on PATH |
| man/Makefile + tools/ronnjs (man-page generator) | build-tool | 186 | man/Makefile L7 defines RONNJS using PKGSRC node | BUILD-TIME ONLY: needs a host node at /opt/loc |
| Native node add-on C++ sources (expat, kstat, libuuid) | native-addon | 0 | Built by src/Makefile link rules L593-603, insta | Raw v8/node 0.10 C++ ABI (node.h, v8.h). 32-bi |
| Vendored external node native packages (dtrace-provider, qlocker/fs-ext, zonename, bunyan) | native-addon | 0 | dtrace-provider/qlocker/zonename built via npm-i | dtrace-provider, qlocker, zonename are tracked |
| Platform node CLIs and daemons (runtime consumers) | cli | 0 | json: BUILT+USRBIN target -> /usr/bin/json (L286 | Every file starts #!/usr/node/bin/node (severa |

## Appendix B — Reuse map (vnext → smartos-live)

| smartos component | repo | coverage | vnext paths | gaps |
|---|---|---|---|---|
| VM.js / vmadm (node) - the create/install/zonecfg engine | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | Brands: only bhyve + joyent-minimal. kvm, lx, joyent (full), |
| vmadm CLI (the /usr/sbin/vmadm node binary front-end) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | Subcommands present: create, start, stop, reboot, delete, co |
| node-vmadm wrapper (how cn-agent/tritonagent drives /usr/sbin/vmadm) + payload builder | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | cn-platform smartos/vmadm.rs (699 LOC) is a SHELLOUT wrapper |
| metadata agent (mdata-get / .zonecontrol/metadata.sock socket protocol) -> REPLACED by AWS IMDSv2-style HTTP listener (re-architecture, not a port) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | This is NOT the SmartOS metadata-agent socket protocol. It i |
| vmadmd (console serving, VNC/serial proxy; the daemon side of vmadm) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | Only the console-serving slice of vmadmd is covered (TLS Web |
| kstat (node kstat native addon used by cn-agent StatusReporter) | mariana-trench | full | /Users/nwilkens/workspace/triton/triton-vnext/mari | Shells out to /usr/bin/kstat -p rather than linking libkstat |
| zfs / zpool (node-zfs module the agent used) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | cn-platform smartos/zfs.rs (1023 LOC) reproduces node-zfs's  |
| sysinfo (/usr/bin/sysinfo CN inventory blob) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | Wraps the EXISTING /usr/bin/sysinfo binary (SYSINFO_BIN) and |
| disklayout / disk inventory + SMART (node disklayout.js + smartos disk tooling) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | disks.rs (492 LOC) is per-disk INVENTORY + SMART health + io |
| zoneevent (/usr/vm/sbin/zoneevent) + /etc/zones fs.watch (cn-agent StatusReporter dirty-flag signals) | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | ZoneeventWatcher SHELLS OUT to /usr/vm/sbin/zoneevent (DEFAU |
| (none in node smartos-live) NIC-tag registration for proteus links — the create-path precondition / 'undefined VNIC' fix | mariana-trench | partial | /Users/nwilkens/workspace/triton/triton-vnext/mari | HostNicTagRegistrar (in backend/host.rs) wraps /usr/bin/nict |
| fwadm / firewall.js / ipf (node-side firewall rule compiler + ipfilter apply) | monitor-reef | none | apis/cloudapi-api/src/types/firewall.rs, cli/trito | No node-side firewall at all: nothing parses Triton fwrule D |
| imgadm import (image install: download zfs-send stream, sha verify, zfs receive, /var/imgadm catalog) | monitor-reef | partial | services/tritonagent/src/images.rs, services/trito | images.rs (496 LOC) deliberately REPLACES `imgadm import` wi |
| tritond image bundle format (Triton-native replacement for imgadm manifest+content tar) | monitor-reef | full | libs/tritond-image-manifest/src/lib.rs | Self-contained, complete for its scope: defines the `tritond |
| imgadm IMGAPI v2 manifest schema (the canonical images.smartos.org manifest imgadm consumes) | monitor-reef | full | libs/imgapi-manifest/src/lib.rs | Complete strict-serde model of the IMGAPI v2 manifest (v/uui |
| imgadm image blob transport (large content payload store; imgadm fetches files[] bytes) | monitor-reef | partial | libs/imgapi-blob-manta/src/lib.rs | S3/Manta-backed blob store for the gzipped zfs-send payload  |
| imgadm fetch-nocloud / cloud-init image import (qcow2/vmdk/xz -> zfs.gz + IMGAPI manifest pipeline) | monitor-reef | full | libs/nocloud-import/src/lib.rs, libs/nocloud-impor | End-to-end vendor cloud-image importer (alma/ubuntu/etc): re |
| qcow2 disk-image decoder (used by image import to read vendor qcow2 cloud images) | monitor-reef | full | libs/qcow/src/lib.rs, libs/qcow/src/reader.rs, lib | Vendored fork of crates.io `qcow` 1.2.0 (panda-re/qcow-rs).  |
| vmdk disk-image decoder (used by image import to read VMware vmdk cloud images) | monitor-reef | full | libs/vmdk/src/vmdk_reader.rs, libs/vmdk/src/header | Vendored/trimmed fork of strozfriedberg/vmdk-rs (crate name  |
| json (the SmartOS jq-like JSON CLI, /usr/bin/json) | monitor-reef | none |  | No reimplementation of the `json` CLI tool. Production code  |
| bunyan (the node-bunyan log pretty-printer CLI) | monitor-reef | partial | services/tritonagent/src/log_tailer.rs, libs/trito | No `bunyan` pretty-printer CLI. What exists is a one-way PAR |
| sysinfo (/usr/bin/sysinfo CN inventory tool) | mariana-trench | partial | libs/tritond-cn-platform/src/smartos/sysinfo.rs, l | Does NOT reimplement sysinfo. `Sysinfo` is a thin adapter th |
| kstat reader (replaces cn-agent's bindings('kstat') native addon) | mariana-trench | partial | libs/tritond-cn-platform/src/smartos/kstat.rs | Explicit comment: 'Rust has no production-ready libkstat cra |
| zoneevent (/usr/vm/sbin/zoneevent zone-state stream) + /etc/zones watcher | mariana-trench | partial | libs/tritond-cn-platform/src/cn_status/watchers.rs | `ZoneeventWatcher` reproduces the cn-agent watcher loop but  |
| disklayout (CN disk topology -> zpool layout planner) | monitor-reef | none |  | Zero hits for disklayout in either repo's real source. There |
| mkzpool (create the zones zpool during CN setup) | monitor-reef | none |  | No zpool CREATE anywhere. All zpool usage is read-only telem |
| filewait (block until a file/path appears) | monitor-reef | none |  | Zero hits in either repo. No filewait equivalent binary. Ad- |
| zoneevent binary itself / net-boot-config (boot-time network config from boot params) | monitor-reef | none |  | net-boot-config / net_boot_config: zero hits in either repo. |
| ntp_config (generate /etc/inet/ntp.conf from CN config) | monitor-reef | none |  | Zero hits for ntp_config / ntp.conf in either repo. No NTP c |
| Native addon: kstat (Node bindings('kstat').Reader) | mariana-trench | partial | libs/tritond-cn-platform/src/smartos/kstat.rs | vnext does NOT use a Rust kstat crate and does NOT FFI into  |
| Native addon: zonename / libuuid (UUID generation + current zone name) | monitor-reef | partial | libs/tritond-vmadm/src/payload/normalize.rs, libs/ | libuuid: NOT used. UUIDs come from the pure-Rust crates.io ` |
| Native addon: expat (XML parsing, used by vmadm/SMF manifest paths) | monitor-reef | none |  | No expat, and no XML parser crate (no quick-xml / quick_xml  |
| Native addon: dtrace-provider / USDT probes | monitor-reef | none |  | No USDT probes. Zero `usdt` and zero `dtrace-provider` in ei |
| Native addon: qlocker (cross-process advisory file lock) | monitor-reef | none |  | No qlocker and no equivalent file-lock crate (no fs2 / fd-lo |

## Appendix C — Golden captures (10.88.88.102)

Saved under the session scratchpad `golden/`; re-capture before each parity gate.

| command | file | ok |
|---|---|---|
| `vmadm list -p` | vmadm_list_p.txt | yes |
| `vmadm list -j` | vmadm_list_j.json | NO |
| `vmadm lookup -j` | vmadm_lookup_j.json | yes |
| `vmadm get 4256eeee-c8b0-4fed-9e0a-975a31e14571` | vmadm_get_first.json | yes |
| `imgadm list -j` | imgadm_list_j.json | yes |
| `imgadm sources` | imgadm_sources.txt | yes |
| `fwadm list -p` | fwadm_list_p.txt | yes |
| `fwadm stats` | fwadm_stats.txt | NO |
| `fwadm show` | fwadm_show.txt | NO |
| `fwadm help` | fwadm_help.txt | yes |
| `sysinfo` | sysinfo.json | yes |
| `sysinfo -p` | sysinfo_p.txt | yes |
| `nictagadm list -p` | nictagadm_list_p.txt | yes |
| `/usr/bin/json --version` | json_version.txt | yes |
| `echo '{"a":1}' | json a` | json_sample.txt | yes |
| `bunyan --version` | bunyan_version.txt | yes |
| `/usr/node/bin/node --version` | node_version.txt | yes |
| `disklayout -h (usage)` | disklayout_usage.txt | yes |
| `vminfo (usage)` | vminfo_usage.txt | yes |
| `ls -la /usr/node/node_modules` | node_modules_ls.txt | yes |
| `ls -la /usr/node/node_modules/ (resolved) + /usr/n` | node_modules_resolved.txt | yes |
| `head -40 /usr/vm/node_modules/VM.js` | VM.js_head.txt | yes |
| `grep -n Copyright /usr/vm/node_modules/VM.js | hea` | VM.js_copyright_shipped.txt | yes |
| `grep -n proteus /usr/vm/node_modules/VM.js` | VM.js_proteus_shipped.txt | yes |
| `head -40 /usr/img/sbin/imgadm` | imgadm_head.txt | yes |
| `head -40 /usr/sbin/fwadm` | fwadm_head.txt | yes |
| `digest -a md5 of shipped VM.js/imgadm/fwadm (md5su` | shipped_md5.txt | yes |
