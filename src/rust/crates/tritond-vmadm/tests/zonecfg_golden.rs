// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! Golden tests for the create-time `zonecfg` blob. The expected blobs
//! are hand-traced from node `VM.js` `createZone`/`buildZonecfgUpdate` for
//! representative agent payloads (joyent-minimal legacy-NIC form; bhyve
//! modern-NIC form). They pin the renderer's byte output; the live
//! differential harness (V-7/V-DIFF) validates against real node vmadm.
//!
//! `unwrap`/`expect` are fine in test fixtures.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use tritond_vmadm::build_create_zonecfg;
use tritond_vmadm::payload::{VmadmCreatePayload, normalize};

fn epoch() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(0, 0).unwrap()
}

fn render(json: serde_json::Value) -> String {
    let payload = VmadmCreatePayload::from_json_slice(json.to_string().as_bytes()).unwrap();
    let spec = normalize(payload, epoch()).unwrap();
    build_create_zonecfg(&spec, epoch())
}

#[test]
fn joyent_minimal_create_blob() {
    let got = render(serde_json::json!({
        "uuid": "11111111-1111-1111-1111-111111111111",
        "brand": "joyent-minimal",
        "image_uuid": "22222222-2222-2222-2222-222222222222",
        "alias": "fdb0",
        "hostname": "fdb0",
        "max_physical_memory": 2048,
        "max_locked_memory": 2048,
        "max_swap": 4096,
        "tmpfs": 1024,
        "cpu_cap": 200,
        "cpu_shares": 200,
        "quota": 25,
        "resolvers": ["10.199.199.14"],
        "nics": [{
            "interface": "net0", "nic_tag": "admin",
            "ip": "10.199.199.50", "netmask": "255.255.255.0",
            "gateway": "10.199.199.1", "vlan_id": 0, "mtu": 1500,
            "mac": "02:00:00:de:ad:01", "primary": true
        }]
    }));

    let want = "\
create -b
set zonepath=/zones/11111111-1111-1111-1111-111111111111
set brand=joyent-minimal
set uuid=11111111-1111-1111-1111-111111111111
set ip-type=exclusive
add attr; set name=\"provisioning\"; set type=string; set value=true;end
add attr; set name=\"vm-version\"; set type=string; set value=\"1\"; end
add attr; set name=\"create-timestamp\"; set type=string; set value=\"1970-01-01T00:00:00.000Z\"; end
add attr; set name=transition; set value=\"provisioning:running:300000\"; set type=string; end
add attr; set name=\"dataset-uuid\"; set type=string; set value=\"22222222-2222-2222-2222-222222222222\"; end
set cpu-shares=200
set limitpriv=\"default\"
set max-lwps=2000
set max-msg-ids=4096
set max-sem-ids=4096
set max-shm-ids=4096
set max-shm-memory=2147483648
set zfs-io-priority=100
set autoboot=true
add capped-memory; set physical=2048m; set locked=2048m; set swap=4096m; end
add capped-cpu; set ncpus=2; end
add attr; set name=\"billing-id\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"owner-uuid\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"tmpfs\"; set type=string; set value=\"1024\"; end
add attr; set name=\"hostname\"; set type=string; set value=\"fdb0\"; end
add attr; set name=\"dns-domain\"; set type=string; set value=\"local\"; end
add attr; set name=\"resolvers\"; set type=string; set value=\"10.199.199.14\"; end
add attr; set name=\"alias\"; set type=string; set value=\"ZmRiMA==\"; end
add net
set physical=net0
set mac-addr=2:0:0:de:ad:1
set global-nic=admin
add property (name=gateway, value=\"10.199.199.1\")
add property (name=gateways, value=\"10.199.199.1\")
add property (name=netmask, value=\"255.255.255.0\")
add property (name=ip, value=\"10.199.199.50\")
add property (name=ips, value=\"10.199.199.50/24\")
set vlan-id=0
add property (name=mtu, value=\"1500\")
end
select net mac-addr=2:0:0:de:ad:1; add property (name=primary,value=\"true\"); end
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

#[test]
fn lx_create_blob() {
    // lx is an OS zone: same shape as joyent-minimal (default limit_priv,
    // OS rctls, capped-memory, no disks/flex/ram) plus brand=lx and the
    // kernel-version attr. Hand-traced from VM.js createZone with
    // features.type == 'LX' (proptable.js:2451) + setAttr('kernel-version').
    let got = render(serde_json::json!({
        "uuid": "11111111-1111-1111-1111-111111111111",
        "brand": "lx",
        "image_uuid": "4d3ed29a-c851-11e6-b5a9-639aada4a9c8",
        "kernel_version": "4.4.0",
        "alias": "lx0",
        "max_physical_memory": 256,
        "max_locked_memory": 256,
        "max_swap": 256,
        "tmpfs": 256,
        "cpu_shares": 100,
        "quota": 10,
        "nics": [{
            "interface": "net0", "nic_tag": "admin",
            "ip": "10.88.88.50", "netmask": "255.255.255.0",
            "gateway": "10.88.88.2", "mac": "02:00:00:aa:bb:cc", "primary": true
        }]
    }));

    let want = "\
create -b
set zonepath=/zones/11111111-1111-1111-1111-111111111111
set brand=lx
set uuid=11111111-1111-1111-1111-111111111111
set ip-type=exclusive
add attr; set name=\"provisioning\"; set type=string; set value=true;end
add attr; set name=\"vm-version\"; set type=string; set value=\"1\"; end
add attr; set name=\"create-timestamp\"; set type=string; set value=\"1970-01-01T00:00:00.000Z\"; end
add attr; set name=transition; set value=\"provisioning:running:300000\"; set type=string; end
add attr; set name=\"dataset-uuid\"; set type=string; set value=\"4d3ed29a-c851-11e6-b5a9-639aada4a9c8\"; end
set cpu-shares=100
set limitpriv=\"default\"
set max-lwps=2000
set max-msg-ids=4096
set max-sem-ids=4096
set max-shm-ids=4096
set max-shm-memory=268435456
set zfs-io-priority=100
set autoboot=true
add capped-memory; set physical=256m; set locked=256m; set swap=256m; end
add attr; set name=\"billing-id\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"owner-uuid\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"tmpfs\"; set type=string; set value=\"256\"; end
add attr; set name=\"dns-domain\"; set type=string; set value=\"local\"; end
add attr; set name=\"kernel-version\"; set type=string; set value=\"4.4.0\"; end
add attr; set name=\"alias\"; set type=string; set value=\"bHgw\"; end
add net
set physical=net0
set mac-addr=2:0:0:aa:bb:cc
set global-nic=admin
add property (name=gateway, value=\"10.88.88.2\")
add property (name=gateways, value=\"10.88.88.2\")
add property (name=netmask, value=\"255.255.255.0\")
add property (name=ip, value=\"10.88.88.50\")
add property (name=ips, value=\"10.88.88.50/24\")
end
select net mac-addr=2:0:0:aa:bb:cc; add property (name=primary,value=\"true\"); end
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}

#[test]
fn bhyve_create_blob() {
    let got = render(serde_json::json!({
        "uuid": "11111111-1111-1111-1111-111111111111",
        "brand": "bhyve",
        "alias": "web0",
        "hostname": "web0",
        "ram": 2048,
        "vcpus": 2,
        "bootrom": "uefi",
        "flexible_disk_size": 20480,
        "bhyve_extra_opts": "-o migrate.export=true",
        "disks": [{
            "uuid": "44444444-4444-4444-4444-444444444444",
            "boot": true, "model": "virtio",
            "image_uuid": "33333333-3333-3333-3333-333333333333",
            "size": 20480
        }],
        "nics": [{
            "interface": "net0", "nic_tag": "proteus49377", "model": "virtio",
            "mac": "02:00:00:de:ad:02", "ips": ["10.0.0.5/24"],
            "gateways": ["10.0.0.1"], "mtu": 1500, "primary": true
        }]
    }));

    let want = "\
create -b
set zonepath=/zones/11111111-1111-1111-1111-111111111111
set brand=bhyve
set uuid=11111111-1111-1111-1111-111111111111
set ip-type=exclusive
add attr; set name=\"provisioning\"; set type=string; set value=true;end
add attr; set name=\"vm-version\"; set type=string; set value=\"1\"; end
add attr; set name=\"create-timestamp\"; set type=string; set value=\"1970-01-01T00:00:00.000Z\"; end
add attr; set name=transition; set value=\"provisioning:running:300000\"; set type=string; end
set cpu-shares=100
set limitpriv=\"default,-file_link_any,-net_access,-proc_fork,-proc_info,-proc_session\"
set max-lwps=2000
set max-msg-ids=4096
set max-sem-ids=4096
set max-shm-ids=4096
set max-shm-memory=3489660928
set zfs-io-priority=100
set autoboot=true
add capped-memory; set physical=3328m; set locked=3328m; set swap=3328m; end
add attr; set name=\"billing-id\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"owner-uuid\"; set type=string; set value=\"00000000-0000-0000-0000-000000000000\"; end
add attr; set name=\"hostname\"; set type=string; set value=\"web0\"; end
add attr; set name=\"dns-domain\"; set type=string; set value=\"local\"; end
add attr; set name=\"alias\"; set type=string; set value=\"d2ViMA==\"; end
add attr; set name=\"ram\"; set type=string; set value=\"2048\"; end
add attr; set name=\"com1\"; set type=string; set value=\"/dev/zconsole\"; end
add attr; set name=\"com2\"; set type=string; set value=\"socket,/tmp/vm.ttyb\"; end
add attr; set name=\"bootrom\"; set type=string; set value=\"uefi\"; end
add attr; set name=\"bhyve-extra-opts\"; set type=string; set value=\"-o migrate.export=true\"; end
add attr; set name=\"zlog-mode\"; set type=string; set value=\"g--\"; end
add attr; set name=\"zlog-name\"; set type=string; set value=\"platform.log\"; end
add attr; set name=\"vcpus\"; set type=string; set value=\"2\"; end
add attr; set name=\"flexible-disk-size\"; set type=string; set value=\"20480\"; end
add device
set match=/dev/zvol/rdsk/zones/11111111-1111-1111-1111-111111111111/disk0
add property (name=boot, value=\"true\")
add property (name=model, value=\"virtio\")
add property (name=size, value=\"20480\")
add property (name=image-uuid, value=\"33333333-3333-3333-3333-333333333333\")
add property (name=pci-slot, value=\"0:4:0\")
add property (name=uuid, value=\"44444444-4444-4444-4444-444444444444\")
end
add net
set physical=net0
set mac-addr=2:0:0:de:ad:2
set global-nic=proteus49377
add property (name=gateway, value=\"10.0.0.1\")
add property (name=gateways, value=\"10.0.0.1\")
add property (name=netmask, value=\"255.255.255.0\")
add property (name=ip, value=\"10.0.0.5\")
add property (name=ips, value=\"10.0.0.5/24\")
add property (name=model, value=\"virtio\")
add property (name=mtu, value=\"1500\")
end
select net mac-addr=2:0:0:de:ad:2; add property (name=primary,value=\"true\"); end
";
    assert_eq!(got, want, "\n--- got ---\n{got}\n--- want ---\n{want}");
}
