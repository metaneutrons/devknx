// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::num::NonZeroU32;
use std::process::Stdio;
use std::time::Duration;

use devknx::storage::CaptureStore;
use knx_rs_ip::DeviceServer;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::process::Command;

async fn send(stdin: &mut tokio::process::ChildStdin, message: &Value) {
    stdin
        .write_all(message.to_string().as_bytes())
        .await
        .unwrap();
    stdin.write_all(b"\n").await.unwrap();
    stdin.flush().await.unwrap();
}

async fn receive(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    id: u64,
) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = lines.next_line().await.unwrap().expect("MCP server exited");
            let value: Value = serde_json::from_str(&line).expect("JSON-RPC line");
            if value["id"] == id {
                return value;
            }
        }
    })
    .await
    .expect("MCP response timeout")
}

#[tokio::test]
async fn stdio_handshake_lists_read_only_tools_and_hides_write_by_default() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("mcp.sqlite");
    CaptureStore::open(&database, NonZeroU32::new(10).unwrap()).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(["mcp", "--database", database.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "devknx-test", "version": "1" } }
        }),
    )
    .await;
    let initialized = receive(&mut lines, 1).await;
    assert_eq!(initialized["result"]["serverInfo"]["name"], "devknx");
    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await;
    let listed = receive(&mut lines, 2).await;
    let tools = listed["result"]["tools"].as_array().expect("tool list");
    assert_eq!(tools.len(), 11);
    for name in ["knx_sessions", "knx_connect", "knx_disconnect"] {
        assert!(tools.iter().any(|tool| tool["name"] == name));
    }
    assert!(!tools.iter().any(|tool| tool["name"] == "knx_typed_write"));
    assert!(tools.iter().any(|tool| tool["name"] == "knx_list_captures"));
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == "knx_list_routing_losses")
    );
    assert!(
        !tools
            .iter()
            .any(|tool| tool["name"].as_str().unwrap().contains("raw"))
    );

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "knx_status", "arguments": {}
    } }),
    )
    .await;
    let status = receive(&mut lines, 3).await;
    assert!(status["result"]["structuredContent"]["capture_owner"].is_null());

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/call", "params": {
        "name": "knx_write_preview", "arguments": {
            "address": "1/2/3", "dpt": "1.001", "value": "true"
        }
    } }),
    )
    .await;
    let preview = receive(&mut lines, 4).await;
    assert_eq!(preview["result"]["structuredContent"]["transmitted"], false);

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/call", "params": {
        "name": "knx_typed_write", "arguments": { "address": "1/2/3", "value": "true" }
    } }),
    )
    .await;
    let rejected = receive(&mut lines, 5).await;
    assert!(rejected["error"].is_object());

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "one stdio scenario verifies the full offline connect and disconnect lifecycle"
)]
async fn stdio_can_connect_and_disconnect_before_the_capture_database_exists() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("new-capture.sqlite");
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    let mut child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(["mcp", "--database", database.to_str().unwrap()])
        .env("HOME", directory.path())
        .env("XDG_DATA_HOME", directory.path())
        .env("LOCALAPPDATA", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                "clientInfo": { "name": "devknx-test", "version": "1" } }
        }),
    )
    .await;
    assert_eq!(
        receive(&mut lines, 1).await["result"]["serverInfo"]["name"],
        "devknx"
    );
    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    )
    .await;
    assert!(!database.exists());

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "knx_sessions", "arguments": {}
    } }),
    )
    .await;
    assert!(
        receive(&mut lines, 2).await["result"]["structuredContent"]["sessions"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {
        "name": "knx_connect", "arguments": { "endpoint": endpoint }
    } }),
    )
    .await;
    let connected = receive(&mut lines, 3).await;
    assert_eq!(
        connected["result"]["structuredContent"]["session"]["endpoint"],
        endpoint
    );
    assert!(database.exists());
    tokio::time::timeout(Duration::from_secs(10), async {
        for id in 4..50 {
            send(
                &mut stdin,
                &json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {
                "name": "knx_status", "arguments": {}
            } }),
            )
            .await;
            let status = receive(&mut lines, id).await;
            if status["result"]["structuredContent"]["capture_owner"]["value"]["state"]
                == "connected"
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("MCP-owned KNX session did not connect");
    })
    .await
    .expect("MCP connection became ready");

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 50, "method": "tools/call", "params": {
        "name": "knx_disconnect", "arguments": {}
    } }),
    )
    .await;
    let disconnected = receive(&mut lines, 50).await;
    assert_eq!(
        disconnected["result"]["structuredContent"]["disconnected"],
        true
    );
    assert_eq!(
        disconnected["result"]["structuredContent"]["session"]["endpoint"],
        endpoint
    );

    drop(stdin);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    let stopped = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(["daemon", "--stop"])
        .env("HOME", directory.path())
        .env("XDG_DATA_HOME", directory.path())
        .env("LOCALAPPDATA", directory.path())
        .output()
        .await
        .unwrap();
    assert!(stopped.status.success());
}
