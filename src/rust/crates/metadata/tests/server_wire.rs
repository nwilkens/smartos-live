// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.
//
// Copyright 2026 Edgecast Cloud LLC.

//! End-to-end wire test: run the real OS-zone unix server against an in-memory
//! cache seeded with the img2 golden VM object, then drive it with a client
//! over a real unix socket and assert byte-exact V1/V2 responses.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use metadata::cache::VmCache;
use metadata::crc32::crc32_calc;
use metadata::handler::ReadOnlyWriter;
use metadata::server::serve_os_zone;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::watch;

fn img2() -> serde_json::Value {
    json!({
        "zonename": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
        "uuid": "4256eeee-c8b0-4fed-9e0a-975a31e14571",
        "brand": "joyent-minimal",
        "hostname": "img2",
        "alias": "img2",
        "dns_domain": "local",
        "resolvers": ["10.199.199.14"],
        "nics": [{"interface":"net0","mac":"02:d3:c4:c5:52:13","vlan_id":0,"nic_tag":"admin","netmask":"255.255.255.0","ip":"10.10.1.3","ips":["10.10.1.3/24"],"mtu":1500,"primary":true}],
        "routes": {},
        "customer_metadata": {},
        "internal_metadata": {"root_pw": "secret"},
        "tags": {}
    })
}

fn v2_get(reqid: &str, key: &str) -> String {
    let arg = B64.encode(key.as_bytes());
    let inner = format!("{reqid} GET {arg}");
    format!("V2 {} {} {}\n", inner.len(), crc32_calc(inner.as_bytes()), inner)
}

fn v2_keys(reqid: &str) -> String {
    let inner = format!("{reqid} KEYS");
    format!("V2 {} {} {}\n", inner.len(), crc32_calc(inner.as_bytes()), inner)
}

async fn read_one(stream: &mut UnixStream) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf))
            .await
            .expect("read timeout")
            .expect("read");
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
        if out.ends_with(b"\n") {
            break;
        }
    }
    out
}

#[tokio::test]
async fn os_zone_v1_and_v2_wire_bytes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sock = dir.path().join("md-test.sock");

    let cache = VmCache::new();
    cache.upsert(&img2());
    let (_tx, rx) = watch::channel(false);

    let sp = sock.clone();
    let zn = "4256eeee-c8b0-4fed-9e0a-975a31e14571".to_string();
    let handle = tokio::spawn(async move {
        serve_os_zone(sp, zn, cache, std::sync::Arc::new(ReadOnlyWriter), rx).await;
    });

    // Wait for the socket to appear.
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut s = UnixStream::connect(&sock).await.expect("connect");

    // NEGOTIATE V2 -> V2_OK
    s.write_all(b"NEGOTIATE V2\n").await.expect("write");
    assert_eq!(read_one(&mut s).await, b"V2_OK\n");

    // V1 GET sdc:hostname -> SUCCESS\nimg2\n.\n
    s.write_all(b"GET sdc:hostname\n").await.expect("write");
    assert_eq!(read_one(&mut s).await, b"SUCCESS\nimg2\n.\n");

    // V2 GET sdc:hostname (reqid 000001) -> exact live bytes.
    s.write_all(v2_get("000001", "sdc:hostname").as_bytes()).await.expect("write");
    assert_eq!(read_one(&mut s).await, b"V2 23 0cb469ba 000001 SUCCESS aW1nMg==\n");

    // V2 GET sdc:nonexistent (reqid 000002) -> NOTFOUND.
    s.write_all(v2_get("000002", "sdc:nonexistent-key-xyz").as_bytes()).await.expect("write");
    assert_eq!(read_one(&mut s).await, b"V2 15 1ddcd766 000002 NOTFOUND\n");

    // V2 KEYS (reqid 000003) -> root_pw.
    s.write_all(v2_keys("000003").as_bytes()).await.expect("write");
    assert_eq!(read_one(&mut s).await, b"V2 27 40a81145 000003 SUCCESS cm9vdF9wdw==\n");

    // V2 GET sdc:resolvers -> JSON array.
    s.write_all(v2_get("000005", "sdc:resolvers").as_bytes()).await.expect("write");
    let resp = read_one(&mut s).await;
    let txt = String::from_utf8(resp).expect("utf8");
    // decode the base64 body and check the value.
    let b64 = txt.trim_end().rsplit(' ').next().expect("body");
    let decoded = String::from_utf8(B64.decode(b64).expect("b64")).expect("utf8");
    assert_eq!(decoded, "[\"10.199.199.14\"]");

    // V2 GET sdc:nics -> byte-exact JSON with field order.
    s.write_all(v2_get("000004", "sdc:nics").as_bytes()).await.expect("write");
    let resp = read_one(&mut s).await;
    let txt = String::from_utf8(resp).expect("utf8");
    let b64 = txt.trim_end().rsplit(' ').next().expect("body");
    let decoded = String::from_utf8(B64.decode(b64).expect("b64")).expect("utf8");
    assert_eq!(
        decoded,
        "[{\"interface\":\"net0\",\"mac\":\"02:d3:c4:c5:52:13\",\"vlan_id\":0,\"nic_tag\":\"admin\",\"netmask\":\"255.255.255.0\",\"ip\":\"10.10.1.3\",\"ips\":[\"10.10.1.3/24\"],\"mtu\":1500,\"primary\":true}]"
    );

    drop(s);
    handle.abort();
}
