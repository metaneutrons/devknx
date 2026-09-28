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
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

struct Process(Child);

impl Process {
    fn start(args: &[&str]) -> Self {
        Self(
            Command::new(env!("CARGO_BIN_EXE_devknx"))
                .args(args)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("start devknx process"),
        )
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn cli(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(args)
        .output()
        .expect("run devknx CLI")
}

async fn http(address: SocketAddr, body: &Value) -> io::Result<(u16, Value)> {
    let mut stream = TcpStream::connect(address).await?;
    let body = body.to_string();
    let request = format!(
        "POST /v1/operations/typed-write HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len(),
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| io::Error::other("missing HTTP headers"))?;
    let code = String::from_utf8_lossy(&response[..header_end])
        .split_whitespace()
        .nth(1)
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| io::Error::other("invalid HTTP status"))?;
    let payload = &response[header_end + 4..];
    let body = serde_json::from_slice(payload)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(payload).into_owned()));
    Ok((code, body))
}

struct McpPeer {
    _child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
}

impl McpPeer {
    async fn start(database: &Path) -> Self {
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(["mcp", "--database", database.to_str().unwrap()])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut peer = Self {
            _child: child,
            stdin,
            lines,
            id: 0,
        };
        let init = peer
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-11-25", "capabilities": {},
                    "clientInfo": { "name": "devknx-policy-test", "version": "1" }
                }),
            )
            .await;
        assert_eq!(init["result"]["serverInfo"]["name"], "devknx");
        peer.send(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
            .await;
        peer
    }

    async fn send(&mut self, value: &Value) {
        self.stdin
            .write_all(value.to_string().as_bytes())
            .await
            .unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        let id = self.id;
        self.send(&json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }))
            .await;
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("MCP process exited");
                let value: Value = serde_json::from_str(&line).unwrap();
                if value["id"] == id {
                    return value;
                }
            }
        })
        .await
        .expect("MCP response timeout")
    }

    async fn write(&mut self, dpt: &str, value: &str) -> Value {
        self.request(
            "tools/call",
            json!({
                "name": "knx_typed_write",
                "arguments": { "address": "1/2/5", "dpt": dpt, "value": value }
            }),
        )
        .await
    }
}

async fn wait_for_owner(owner: &mut Process, database: &Path) {
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
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("owner connected");
}

async fn wait_for_api(api: &mut Process, address: SocketAddr) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            assert!(api.0.try_wait().unwrap().is_none(), "REST process exited");
            if TcpStream::connect(address).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("REST listener ready");
}

async fn next_bus_write(gateway: &mut DeviceServer) {
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), gateway.recv())
            .await
            .unwrap(),
        Some(ServerEvent::TunnelFrame(_))
    ));
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered live scenario compares all three adapters and audits"
)]
async fn cli_rest_and_mcp_cannot_bypass_dpt_policy_and_audit_distinct_origins() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("policy.sqlite");
    let database_str = database.to_str().unwrap();
    let xml = directory.path().join("groups.xml");
    std::fs::write(&xml, "<GroupAddress-Export xmlns=\"http://knx.org/xml/ga-export/01\"><GroupRange Name=\"Test\"><GroupAddress Name=\"Lamp\" Address=\"1/2/5\" DPTs=\"DPST-1-1\" /></GroupRange></GroupAddress-Export>").unwrap();
    let imported = cli(&[
        "ets-import",
        xml.to_str().unwrap(),
        "--database",
        database_str,
        "--format",
        "xml",
    ]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );

    let mut gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let mut owner = Process::start(&["serve", &endpoint, "--database", database_str]);
    wait_for_owner(&mut owner, &database).await;
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let mut api = Process::start(&[
        "api",
        "--database",
        database_str,
        "--bind",
        &address.to_string(),
    ]);
    wait_for_api(&mut api, address).await;
    let mut mcp = McpPeer::start(&database).await;

    let conflicting = json!({ "address": "1/2/5", "dpt": "5.010", "value": "42" });
    let cli_rejected = cli(&[
        "write",
        "--database",
        database_str,
        "--dpt",
        "5.010",
        "1/2/5",
        "42",
    ]);
    assert!(!cli_rejected.status.success());
    let (code, rest_rejected) = http(address, &conflicting).await.unwrap();
    assert_eq!(code, 422);
    assert!(
        rest_rejected["error"]
            .as_str()
            .unwrap()
            .contains("not declared")
    );
    let mcp_rejected = mcp.write("5.010", "42").await;
    assert_eq!(mcp_rejected["result"]["isError"], true);
    assert!(
        mcp_rejected["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap()
            .contains("not declared")
    );
    let smuggled_raw = json!({ "address": "1/2/5", "dpt": "1.001", "value": "true", "payload": { "form": "inline", "value": 1 } });
    let (code, _) = http(address, &smuggled_raw).await.unwrap();
    assert_ne!(code, 200);
    let mcp_raw = mcp
        .request(
            "tools/call",
            json!({
                "name": "knx_typed_write", "arguments": smuggled_raw
            }),
        )
        .await;
    assert!(mcp_raw["error"].is_object() || mcp_raw["result"]["isError"] == true);
    assert!(
        tokio::time::timeout(Duration::from_millis(150), gateway.recv())
            .await
            .is_err()
    );
    assert!(
        CaptureStore::open_existing(&database)
            .unwrap()
            .read_operation_audit_after(0, NonZeroU32::new(10).unwrap())
            .unwrap()
            .is_empty()
    );

    let preview = cli(&[
        "write-preview",
        "--database",
        database_str,
        "--dpt",
        "1.001",
        "1/2/5",
        "true",
    ]);
    assert!(preview.status.success());
    let preview: Value = serde_json::from_slice(&preview.stdout).unwrap();
    let expected = preview["raw_cemi"].as_str().unwrap();

    let write_database = database_str.to_owned();
    let cli_sent = tokio::task::spawn_blocking(move || {
        cli(&[
            "write",
            "--database",
            &write_database,
            "--dpt",
            "1.001",
            "1/2/5",
            "true",
        ])
    })
    .await
    .unwrap();
    assert!(
        cli_sent.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_sent.stderr)
    );
    let cli_sent: Value = serde_json::from_slice(&cli_sent.stdout).unwrap();
    assert_eq!(cli_sent["raw_cemi"], expected);
    next_bus_write(&mut gateway).await;

    let valid = json!({ "address": "1/2/5", "dpt": "1.001", "value": "true" });
    let (code, rest_sent) = http(address, &valid).await.unwrap();
    assert_eq!(code, 200);
    assert_eq!(rest_sent["raw_cemi"], expected);
    next_bus_write(&mut gateway).await;

    let mcp_sent = mcp.write("1.001", "true").await;
    assert_eq!(mcp_sent["result"]["isError"], false);
    assert_eq!(
        mcp_sent["result"]["structuredContent"]["receipt"]["raw_cemi"],
        expected
    );
    next_bus_write(&mut gateway).await;

    let audit = CaptureStore::open_existing(&database)
        .unwrap()
        .read_operation_audit_after(0, NonZeroU32::new(10).unwrap())
        .unwrap();
    assert_eq!(audit.len(), 3);
    assert_eq!(
        audit
            .iter()
            .map(|entry| entry.origin.as_str())
            .collect::<Vec<_>>(),
        ["local_ipc", "rest_loopback", "mcp_stdio"]
    );
    assert!(
        audit
            .iter()
            .all(|entry| entry.kind == "typed_write" && entry.raw_cemi == expected)
    );
    gateway.stop().await;
}
