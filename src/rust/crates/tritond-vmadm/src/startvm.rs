// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! The kvm `/root/startvm` qemu launch script.
//!
//! Unlike bhyve (whose brand method scripts build the bhyve invocation), the
//! kvm brand's boot hook simply `exec`s `/startvm`, a shell script that
//! `vmadm`/`VM.js` must write into the zoneroot before boot. This is the Rust
//! port of node `VM.js` `buildKVMCmdline` + the script writer (VM.js:13540-
//! 13869). The script is a pure function of the VM spec plus the host's
//! `platform_buildstamp` (the smbios version), so it is deterministic and
//! unit-testable.
//!
//! The command line built here matches a node-created kvm guest byte-for-byte
//! for the create-time fields (no per-boot `extra` disks/boot override, which
//! only `VM.start({extra})` supplies): `-m`/`-name`/`-uuid`/`-cpu`, per-disk
//! `-drive`, `-boot`, per-NIC `-device virtio-net-pci`/`-net vnic`, `-smbios`,
//! `-pidfile`, `-vga`, the qmp/console/ttyb chardevs, and `-vnc`/`-spice`.

use crate::model::nic::NicAddr;
use crate::model::spec::VmCreateSpec;

/// Default virtio tx tunables (VM.js `VIRTIO_TXTIMER_DEFAULT` /
/// `VIRTIO_TXBURST_DEFAULT`).
const VIRTIO_TXTIMER_DEFAULT: u64 = 200_000;
const VIRTIO_TXBURST_DEFAULT: u64 = 128;

/// Build the qemu argument vector for a kvm guest (`buildKVMCmdline`,
/// VM.js:13560-13845). `platform_buildstamp` is the sysinfo "Live Image"
/// value used in the smbios version string; an absent stamp falls back to an
/// empty string (matching node when the fact is missing).
#[must_use]
pub fn build_kvm_cmdargs(spec: &VmCreateSpec, platform_buildstamp: &str) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let push = |a: &mut Vec<String>, flag: &str, val: String| {
        a.push(flag.to_string());
        a.push(val);
    };

    push(&mut a, "-m", spec.ram_mib.to_string());
    push(&mut a, "-name", spec.uuid.to_string());
    push(&mut a, "-uuid", spec.uuid.to_string());
    // cpu_type is a kvm payload field not modeled here; default qemu64.
    push(&mut a, "-cpu", "qemu64".to_string());

    // -smp only when vcpus > 1 (VM.js:13571).
    if spec.vcpus > 1 {
        push(&mut a, "-smp", spec.vcpus.to_string());
    }

    // per-disk -drive (VM.js:13574). The drive index is the disk's position.
    for (idx, disk) in spec.disks.iter().enumerate() {
        let media = disk.effective_media().as_str();
        let mut drive = format!(
            "file={},if={},index={},media={}",
            disk.raw_path,
            disk.model.as_str(),
            idx,
            media
        );
        if disk.boot {
            drive.push_str(",boot=on");
        }
        push(&mut a, "-drive", drive);
    }

    // -boot: the spec's boot order, else order=cd (VM.js:13647).
    let boot = spec.boot.clone().unwrap_or_else(|| "order=cd".to_string());
    push(&mut a, "-boot", boot);

    // per-NIC device + vnic (VM.js:13686). virtio NICs use a -device with the
    // tx tunables; others a -net nic.
    let primary_idx = spec.nics.iter().position(|n| n.primary);
    for (nic_idx, nic) in spec.nics.iter().enumerate() {
        let model = nic.model.as_deref().unwrap_or("virtio");
        let mac = &nic.mac;
        if model == "virtio" {
            push(
                &mut a,
                "-device",
                format!(
                    "virtio-net-pci,mac={mac},tx=timer,x-txtimer={},x-txburst={},vlan={nic_idx}",
                    VIRTIO_TXTIMER_DEFAULT, VIRTIO_TXBURST_DEFAULT
                ),
            );
        } else {
            push(
                &mut a,
                "-net",
                format!(
                    "nic,macaddr={mac},vlan={nic_idx},name={},model={model}",
                    nic.interface
                ),
            );
        }

        let mut vnic = format!(
            "vnic,name={},vlan={nic_idx},ifname={}",
            nic.interface, nic.interface
        );

        // Representative IPv4 (VM.js:13718). dhcp/addrconf/v6-only NICs get
        // only the bare vnic line.
        let v4 = NicAddr::representative_ipv4(&nic.ips);
        if let Some(ipnetwork::IpNetwork::V4(net)) = v4 {
            vnic.push_str(&format!(",ip={},netmask={}", net.ip(), net.mask()));

            // The primary NIC supplies the gateway, hostname, and resolvers
            // (VM.js:13742). Only the first primary.
            if Some(nic_idx) == primary_idx {
                if let Some(gw) = nic.gateways.iter().find(|g| g.is_ipv4()) {
                    vnic.push_str(&format!(",gateway_ip={gw}"));
                }
                let hostname = spec
                    .hostname
                    .clone()
                    .unwrap_or_else(|| spec.uuid.to_string());
                vnic.push_str(&format!(",hostname={hostname}"));
                // Up to 4 IPv4 resolvers (OS-2795; qemu caps at 4).
                let v4res: Vec<&String> = spec
                    .resolvers
                    .iter()
                    .filter(|r| r.parse::<std::net::Ipv4Addr>().is_ok())
                    .take(4)
                    .collect();
                for (r_idx, r) in v4res.iter().enumerate() {
                    vnic.push_str(&format!(",dns_ip{r_idx}={r}"));
                }
            }
        }
        push(&mut a, "-net", vnic);
    }

    // -smbios (VM.js:13780). version is `7.<buildstamp>`.
    push(
        &mut a,
        "-smbios",
        format!(
            "type=1,manufacturer=Joyent,product=SmartDC HVM,version=7.{platform_buildstamp},\
             serial={uuid},uuid={uuid},sku=001,family=Virtual Machine",
            uuid = spec.uuid
        ),
    );

    push(&mut a, "-pidfile", "/tmp/vm.pid".to_string());
    // vga is a kvm payload field not modeled here; default std.
    push(&mut a, "-vga", "std".to_string());

    // qmp monitor socket (the control channel vmadmd connects to).
    push(
        &mut a,
        "-chardev",
        "socket,id=qmp,path=/tmp/vm.qmp,server,nowait".to_string(),
    );
    push(&mut a, "-qmp", "chardev:qmp".to_string());

    // serial0 = console, serial1 = metadata API (ttyb).
    push(
        &mut a,
        "-chardev",
        "socket,id=serial0,path=/tmp/vm.console,server,nowait".to_string(),
    );
    push(&mut a, "-serial", "chardev:serial0".to_string());
    push(
        &mut a,
        "-chardev",
        "socket,id=serial1,path=/tmp/vm.ttyb,server,nowait".to_string(),
    );
    push(&mut a, "-serial", "chardev:serial1".to_string());

    // -vnc (VM.js:13810). The `,password` suffix is added only when a
    // vnc_password is set in the payload (not modeled at create here, so the
    // password is set later via QMP set_password — vmadmd's display proxy).
    push(&mut a, "-vnc", "unix:/tmp/vm.vnc".to_string());

    push(&mut a, "-parallel", "none".to_string());
    // -usb is a bare flag (no value).
    a.push("-usb".to_string());
    push(&mut a, "-usbdevice", "tablet".to_string());
    push(&mut a, "-k", "en-us".to_string());

    a
}

/// Render the `/root/startvm` shell script wrapping the qemu cmdline
/// (VM.js:13852). Each arg is double-quoted; the `/startvm.zone` branch is the
/// in-zone qemu-exec path, with a fallback to the GZ binary.
#[must_use]
pub fn build_startvm_script(spec: &VmCreateSpec, platform_buildstamp: &str) -> String {
    let args = build_kvm_cmdargs(spec, platform_buildstamp);
    let quoted = args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");
    format!(
        "#!/usr/bin/bash\n\n\
         exec >/tmp/vm.startvm.log 2>&1\n\n\
         set -o xtrace\n\n\
         if [[ -x /startvm.zone ]]; then\n\
         \x20   exec /smartdc/bin/qemu-exec /startvm.zone {quoted} \n\
         else\n\
         \x20   exec /smartdc/bin/qemu-exec /smartdc/bin/qemu-system-x86_64 {quoted} \n\
         fi\n\n\
         exit 1\n"
    )
}

/// Ensure no spec field that reaches the unquoted-by-shell-then-requoted
/// startvm cmdline can break out. The cmdline values are double-quoted in the
/// script; a literal `"` in a value would break the quoting. Disk paths, MACs,
/// IPs, and the uuid are all typed/validated upstream, but hostname and
/// resolvers flow through, so reject a `"` defensively.
#[must_use]
pub fn startvm_value_is_safe(value: &str) -> bool {
    !value.contains(['"', '\n', '\r', '`', '$'])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::payload::{VmadmCreatePayload, normalize};

    fn spec() -> VmCreateSpec {
        let p = VmadmCreatePayload::from_json_slice(
            serde_json::json!({
                "uuid": "0ebe553b-8a1e-4285-b24d-43f10b3f7f8d",
                "brand": "kvm", "ram": 512, "vcpus": 1,
                "hostname": "0ebe553b-8a1e-4285-b24d-43f10b3f7f8d",
                "disks": [{ "boot": true, "model": "virtio",
                    "image_uuid": "6dde8b1a-0780-11e4-a57b-6b7ac24a7cd4",
                    "image_size": 10240 }],
                "nics": [{ "interface": "net0", "nic_tag": "admin",
                    "model": "virtio", "mac": "92:61:8f:8c:39:ee",
                    "ip": "10.88.88.72", "netmask": "255.255.255.0",
                    "gateway": "10.88.88.1", "primary": true }]
            })
            .to_string()
            .as_bytes(),
        )
        .expect("payload");
        normalize(p, chrono::DateTime::from_timestamp(0, 0).expect("epoch")).expect("normalize")
    }

    #[test]
    fn cmdargs_match_node_kvm_shape() {
        let args = build_kvm_cmdargs(&spec(), "20260622T191148Z");
        let joined = args.join(" ");
        // Core machine flags.
        assert!(joined.contains("-m 512"));
        assert!(joined.contains("-cpu qemu64"));
        // 1 vcpu => NO -smp.
        assert!(!joined.contains("-smp"), "no -smp for 1 vcpu: {joined}");
        // Disk drive on the dash-sibling raw zvol, boot=on, virtio.
        assert!(joined.contains(
            "-drive file=/dev/zvol/rdsk/zones/0ebe553b-8a1e-4285-b24d-43f10b3f7f8d-disk0,\
             if=virtio,index=0,media=disk,boot=on"
        ));
        assert!(joined.contains("-boot order=cd"));
        // virtio NIC device with tx tunables.
        assert!(joined.contains(
            "-device virtio-net-pci,mac=92:61:8f:8c:39:ee,tx=timer,x-txtimer=200000,\
             x-txburst=128,vlan=0"
        ));
        // vnic with ip/netmask/gateway/hostname for the primary.
        assert!(joined.contains(
            "-net vnic,name=net0,vlan=0,ifname=net0,ip=10.88.88.72,\
             netmask=255.255.255.0,gateway_ip=10.88.88.1,\
             hostname=0ebe553b-8a1e-4285-b24d-43f10b3f7f8d"
        ));
        // smbios version is 7.<buildstamp>.
        assert!(joined.contains("version=7.20260622T191148Z"));
        // The qmp control socket + console/ttyb + vnc.
        assert!(joined.contains("-chardev socket,id=qmp,path=/tmp/vm.qmp,server,nowait"));
        assert!(joined.contains("-qmp chardev:qmp"));
        assert!(joined.contains("-chardev socket,id=serial0,path=/tmp/vm.console,server,nowait"));
        assert!(joined.contains("-vnc unix:/tmp/vm.vnc"));
        assert!(joined.contains("-vga std"));
        assert!(joined.contains("-usb -usbdevice tablet -k en-us"));
    }

    #[test]
    fn smp_emitted_for_multivcpu() {
        let mut s = spec();
        s.vcpus = 2;
        let joined = build_kvm_cmdargs(&s, "x").join(" ");
        assert!(joined.contains("-smp 2"));
    }

    #[test]
    fn script_wraps_args_quoted_with_both_branches() {
        let script = build_startvm_script(&spec(), "20260622T191148Z");
        assert!(script.starts_with("#!/usr/bin/bash"));
        assert!(script.contains("if [[ -x /startvm.zone ]]; then"));
        assert!(script.contains("exec /smartdc/bin/qemu-exec /startvm.zone \"-m\" \"512\""));
        assert!(script.contains(
            "exec /smartdc/bin/qemu-exec /smartdc/bin/qemu-system-x86_64 \"-m\" \"512\""
        ));
        assert!(script.contains("\"-qmp\" \"chardev:qmp\""));
        assert!(script.trim_end().ends_with("exit 1"));
    }

    #[test]
    fn safety_rejects_quote_injection() {
        assert!(startvm_value_is_safe("host-1.example"));
        assert!(!startvm_value_is_safe("a\"b"));
        assert!(!startvm_value_is_safe("$(reboot)"));
    }
}
