// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::num::NonZeroU32;
use std::process::Stdio;
use std::time::Duration;

use devknx::storage::CaptureStore;
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
async fn stdio_handshake_lists_structured_tools_and_rejects_unprepared_write() {
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
    assert_eq!(tools.len(), 7);
    assert!(tools.iter().any(|tool| tool["name"] == "knx_typed_write"));
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
    assert_eq!(rejected["result"]["isError"], true);
    assert!(rejected["result"]["structuredContent"]["error"].is_string());

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success());
}
