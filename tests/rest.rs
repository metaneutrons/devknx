// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::io;
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use devknx::ipc::{IpcClient, IpcMessage, WireState};
use devknx::storage::CaptureStore;
use knx_rs_ip::{DeviceServer, ServerEvent};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

struct ChildGuard(Child);

impl ChildGuard {
    fn start(args: &[&str]) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_devknx"))
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start devknx"),
        )
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn http(
    address: SocketAddr,
    method: &str,
    path: &str,
    body: Option<&Value>,
) -> io::Result<(u16, Value)> {
    let mut stream = TcpStream::connect(address).await?;
    let body = body.map_or_else(String::new, Value::to_string);
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        address,
        body.len(),
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("missing HTTP headers"))?;
    let status = String::from_utf8_lossy(&response[..header_end]);
    let code = status
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| io::Error::other("invalid HTTP status"))?;
    let payload = &response[header_end + 4..];
    let data = serde_json::from_slice(payload)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(payload).into_owned()));
    Ok((code, data))
}

async fn owner_ready(owner: &mut ChildGuard, database: &Path) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(
                owner.0.try_wait().unwrap().is_none(),
                "capture owner exited"
            );
            if let Ok(mut client) = IpcClient::connect(database, false).await
                && matches!(
                    client.next().await.unwrap(),
                    Some(IpcMessage::State {
                        value: WireState::Connected { .. },
                        ..
                    })
                )
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("capture owner connected");
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one process-level REST write scenario retains its ordered evidence"
)]
async fn rest_routes_use_capture_owner_dpt_rules_and_origin_audit() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("rest.sqlite");
    let database_str = database.to_str().unwrap();
    let store = CaptureStore::open(&database, NonZeroU32::new(100).unwrap()).unwrap();
    drop(store);

    let mut gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let mut owner = ChildGuard::start(&["serve", &endpoint, "--database", database_str]);
    owner_ready(&mut owner, &database).await;

    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut api = ChildGuard::start(&[
        "api",
        "--database",
        database_str,
        "--bind",
        &address.to_string(),
    ]);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(api.0.try_wait().unwrap().is_none(), "REST process exited");
            if let Ok((200, health)) = http(address, "GET", "/v1/health", None).await {
                assert_eq!(health["capture_owner"]["value"]["state"], "connected");
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("REST listener ready");

    let valid = json!({ "address": "1/2/3", "dpt": "1.001", "value": "true" });
    let (code, preview) = http(address, "POST", "/v1/operations/preview", Some(&valid))
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(preview["transmitted"], false);
    let invalid = json!({ "address": "1/2/3", "value": "true" });
    let (code, _) = http(
        address,
        "POST",
        "/v1/operations/typed-write",
        Some(&invalid),
    )
    .await
    .unwrap();
    assert_eq!(code, 422);
    let raw_smuggling = json!({ "address": "1/2/3", "dpt": "1.001", "value": "true", "payload": { "form": "inline", "value": 1 } });
    let (code, _) = http(
        address,
        "POST",
        "/v1/operations/typed-write",
        Some(&raw_smuggling),
    )
    .await
    .unwrap();
    assert_ne!(code, 200);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), gateway.recv())
            .await
            .is_err()
    );

    let (code, sent) = http(address, "POST", "/v1/operations/typed-write", Some(&valid))
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(sent["type"], "operation_result");
    assert_eq!(sent["raw_cemi"], preview["raw_cemi"]);
    assert!(sent["response_enrichment"].is_null());
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), gateway.recv())
            .await
            .unwrap(),
        Some(ServerEvent::TunnelFrame(_))
    ));
    let audit = CaptureStore::open_existing(&database)
        .unwrap()
        .read_operation_audit_after(0, NonZeroU32::new(10).unwrap())
        .unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].origin, "rest_loopback");
    assert_eq!(audit[0].kind, "typed_write");
    assert_eq!(audit[0].raw_cemi, sent["raw_cemi"]);

    let read = json!({ "address": "1/2/3", "timeout_ms": 1 });
    let (code, receipt) = http(address, "POST", "/v1/operations/read", Some(&read))
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(receipt["read"]["status"], "no_response");
    assert!(receipt["response_enrichment"].is_null());
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), gateway.recv())
            .await
            .unwrap(),
        Some(ServerEvent::TunnelFrame(_))
    ));
    let audit = CaptureStore::open_existing(&database)
        .unwrap()
        .read_operation_audit_after(0, NonZeroU32::new(10).unwrap())
        .unwrap();
    assert_eq!(audit.len(), 2);
    assert_eq!(audit[1].origin, "rest_loopback");
    assert_eq!(audit[1].kind, "read");

    let (code, captures) = http(address, "GET", "/v1/captures?after=0&limit=10", None)
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(captures["items"].as_array().unwrap().len(), 2);
    assert_eq!(captures["next_after"], receipt["capture_id"]);
    let (code, lookup) = http(address, "GET", "/v1/ets/1%2F2%2F3", None)
        .await
        .unwrap();
    assert_eq!(code, 200);
    assert_eq!(lookup["group"], Value::Null);

    gateway.stop().await;
}
