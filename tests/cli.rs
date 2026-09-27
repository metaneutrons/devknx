// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::io::Read as _;
use std::num::NonZeroU32;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use devknx::capture::{CaptureEndpoint, CaptureEvent, RoutingLossEvent};
use devknx::ets::{CsvEncoding, EtsCatalog, EtsFormat};
use devknx::ipc::{IpcClient, IpcMessage, ReadOutcome, WireState};
use devknx::operations::{OperationRequest, RawPayload};
use devknx::storage::CaptureStore;
use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::message::MessageCode;
use knx_rs_core::types::Priority;
use knx_rs_ip::RoutingLostMessage;
use knx_rs_ip::{DeviceServer, ServerEvent};
use rusqlite::{Connection, MAIN_DB};

fn devknx(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(args)
        .output()
        .expect("run devknx")
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").unwrap();
    }
    output
}

fn response_frame() -> CemiFrame {
    CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
        Priority::Low,
        &[0x00, 0x40, 0x2a],
    )
}

#[test]
fn help_describes_available_commands() {
    let output = devknx(&["--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 help");
    assert!(stdout.contains("discover"));
    assert!(stdout.contains("monitor"));
    assert!(stdout.contains("serve"));
    assert!(stdout.contains("api"));
    assert!(stdout.contains("history"));
    assert!(stdout.contains("router-losses"));
    assert!(stdout.contains("export"));
    assert!(stdout.contains("backup"));
    assert!(stdout.contains("status"));
    assert!(stdout.contains("follow"));
    assert!(stdout.contains("ets-import"));
    assert!(stdout.contains("ets-lookup"));
    assert!(stdout.contains("write-preview"));
    assert!(stdout.contains("write-raw"));
    assert!(stdout.contains("audit"));
    #[cfg(feature = "gui")]
    assert!(stdout.contains("gui"));
    #[cfg(not(feature = "gui"))]
    assert!(!stdout.contains("gui"));
    #[cfg(feature = "tui")]
    assert!(stdout.contains("tui"));
    #[cfg(not(feature = "tui"))]
    assert!(!stdout.contains("tui"));
}

#[test]
fn ets_cli_import_lookup_and_invalid_replacement_are_atomic() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("captures.sqlite");
    let xml = directory.path().join("group-addresses.xml");
    let database = database.to_str().expect("UTF-8 path");
    let xml_path = xml.to_str().expect("UTF-8 path");
    std::fs::write(&xml, "<GroupAddress-Export xmlns=\"http://knx.org/xml/ga-export/01\"><GroupRange Name=\"Lighting\"><GroupAddress Name=\"Hall\" Address=\"1/2/3\" Description=\"Main lamp\" DPTs=\"DPST-1-1,DPST-1-6\" /></GroupRange></GroupAddress-Export>").unwrap();
    let imported = devknx(&[
        "ets-import",
        xml_path,
        "--database",
        database,
        "--format",
        "xml",
    ]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    assert!(String::from_utf8_lossy(&imported.stdout).contains("revision=1 groups=1"));
    let lookup = devknx(&["ets-lookup", "--database", database, "1/2/3"]);
    assert!(lookup.status.success());
    let value: serde_json::Value = serde_json::from_slice(&lookup.stdout).unwrap();
    assert_eq!(value["group"]["name"], "Hall");
    assert_eq!(
        value["group"]["dpts"],
        serde_json::json!(["DPST-1-1", "DPST-1-6"])
    );

    std::fs::write(&xml, "<GroupAddress-Export xmlns=\"http://knx.org/xml/ga-export/01\"><GroupRange Name=\"X\"><GroupAddress Name=\"A\" Address=\"1/2/3\" /><GroupAddress Name=\"B\" Address=\"2563\" /></GroupRange></GroupAddress-Export>").unwrap();
    let rejected = devknx(&[
        "ets-import",
        xml_path,
        "--database",
        database,
        "--format",
        "xml",
    ]);
    assert!(!rejected.status.success());
    let unchanged = devknx(&["ets-lookup", "--database", database, "2563"]);
    let value: serde_json::Value = serde_json::from_slice(&unchanged.stdout).unwrap();
    assert_eq!(value["revision"], 1);
    assert_eq!(value["group"]["name"], "Hall");
}

#[tokio::test]
#[expect(
    clippy::too_many_lines,
    reason = "single loopback scenario validates transport, response, timeout and audit in order"
)]
async fn loopback_operations_match_preview_audit_raw_and_observe_read_response_or_timeout() {
    struct CaptureChild(Child);
    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("operations.sqlite");
    let xml = directory.path().join("groups.xml");
    std::fs::write(&xml, "<GroupAddress-Export xmlns=\"http://knx.org/xml/ga-export/01\"><GroupRange Name=\"Test\"><GroupAddress Name=\"Only boolean\" Address=\"1/2/5\" DPTs=\"DPST-1-1\" /></GroupRange></GroupAddress-Export>").unwrap();
    assert!(
        devknx(&[
            "ets-import",
            xml.to_str().unwrap(),
            "--database",
            path.to_str().unwrap(),
            "--format",
            "xml"
        ])
        .status
        .success()
    );
    let mut server = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", server.local_addr());
    let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(["serve", &endpoint, "--database", path.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut child = CaptureChild(child);
    let connected = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = child.0.try_wait().unwrap() {
                panic!("capture owner exited: {status}");
            }
            if let Ok(mut client) = IpcClient::connect(&path, false).await
                && matches!(
                    client.next().await.unwrap(),
                    Some(IpcMessage::State {
                        value: WireState::Connected { .. }
                    })
                )
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(connected.is_ok(), "capture owner did not connect");

    let rejected = IpcClient::operate(
        &path,
        &OperationRequest::TypedWrite {
            address_raw: 0x0a05,
            dpt: Some("5.010".to_owned()),
            value: "42".to_owned(),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(rejected, IpcMessage::OperationError { reason } if reason.contains("not declared"))
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), server.recv())
            .await
            .is_err()
    );

    for (dpt, value) in [
        ("1.001", "true"),
        ("5.010", "42"),
        ("9.001", "21.5"),
        ("13.001", "123456"),
    ] {
        let preview = devknx(&["write-preview", "--dpt", dpt, "1/2/3", value]);
        assert!(
            preview.status.success(),
            "{}",
            String::from_utf8_lossy(&preview.stderr)
        );
        let preview: serde_json::Value = serde_json::from_slice(&preview.stdout).unwrap();
        let result = IpcClient::operate(
            &path,
            &OperationRequest::TypedWrite {
                address_raw: 0x0a03,
                dpt: Some(dpt.to_owned()),
                value: value.to_owned(),
            },
        )
        .await
        .unwrap();
        let transmitted = match result {
            IpcMessage::OperationResult { raw_cemi, .. } => raw_cemi,
            other => panic!("unexpected operation result: {other:?}"),
        };
        assert_eq!(preview["raw_cemi"], transmitted);
        let Some(ServerEvent::TunnelFrame(frame)) =
            tokio::time::timeout(Duration::from_secs(3), server.recv())
                .await
                .unwrap()
        else {
            panic!("server did not receive typed write");
        };
        let wire = hex(frame.as_bytes());
        assert_eq!(wire, transmitted);
    }

    let raw = IpcClient::operate(
        &path,
        &OperationRequest::RawWrite {
            address_raw: 0x0a03,
            payload: RawPayload::Inline(1),
        },
    )
    .await
    .unwrap();
    assert!(matches!(raw, IpcMessage::OperationResult { .. }));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), server.recv())
            .await
            .unwrap(),
        Some(ServerEvent::TunnelFrame(_))
    ));

    let read_path = path.clone();
    let read = tokio::spawn(async move {
        IpcClient::operate(
            &read_path,
            &OperationRequest::Read {
                address_raw: 0x0a03,
                timeout_ms: 1_000,
            },
        )
        .await
        .unwrap()
    });
    let Some(ServerEvent::TunnelFrame(read_frame)) =
        tokio::time::timeout(Duration::from_secs(3), server.recv())
            .await
            .unwrap()
    else {
        panic!("server did not receive read");
    };
    assert_eq!(read_frame.payload(), [0, 0]);
    let unrelated = CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group("1/2/4".parse().unwrap()),
        Priority::Low,
        &[0, 0x40, 1],
    );
    server.send_frame(unrelated).await.unwrap();
    let matching = CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group("1/2/3".parse().unwrap()),
        Priority::Low,
        &[0, 0x40, 1],
    );
    server.send_frame(matching.clone()).await.unwrap();
    let result = read.await.unwrap();
    let matching_hex = hex(matching.as_bytes());
    assert!(
        matches!(result, IpcMessage::OperationResult { read: Some(ReadOutcome::Response { raw_cemi }), .. } if raw_cemi == matching_hex)
    );

    let no_response = IpcClient::operate(
        &path,
        &OperationRequest::Read {
            address_raw: 0x0a03,
            timeout_ms: 50,
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        no_response,
        IpcMessage::OperationResult {
            read: Some(ReadOutcome::NoResponse),
            ..
        }
    ));
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), server.recv())
            .await
            .unwrap(),
        Some(ServerEvent::TunnelFrame(_))
    ));

    let connection = Connection::open(&path).unwrap();
    let raw_count: i64 = connection.query_row("SELECT COUNT(*) FROM operation_audit WHERE kind = 'raw_write' AND status = 'transmitted'", [], |row| row.get(0)).unwrap();
    let typed_count: i64 = connection.query_row("SELECT COUNT(*) FROM operation_audit WHERE kind = 'typed_write' AND status = 'transmitted'", [], |row| row.get(0)).unwrap();
    assert_eq!(raw_count, 1);
    assert_eq!(typed_count, 4);
    let captures = CaptureStore::open_existing(&path)
        .unwrap()
        .read_after(0, NonZeroU32::new(100).unwrap())
        .unwrap();
    assert_eq!(
        captures
            .iter()
            .filter(|row| row.event.direction() == devknx::capture::CaptureDirection::Sent)
            .count(),
        7
    );
    let audit = devknx(&["audit", "--database", path.to_str().unwrap()]);
    assert!(audit.status.success());
    let rows: Vec<serde_json::Value> = String::from_utf8(audit.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 7);
    assert!(
        rows.iter()
            .any(|row| row["kind"] == "raw_write" && row["status"] == "transmitted")
    );
    drop(child);
    server.stop().await;
}

#[test]
fn monitor_rejects_invalid_endpoint_without_connecting() {
    let output = devknx(&["monitor", "http://192.0.2.1:3671"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("unsupported scheme"));
}

#[test]
fn serve_rejects_a_second_capture_writer() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("captures.sqlite");
    let writer = CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero"))
        .expect("create capture store");
    let output = devknx(&[
        "serve",
        "tunnel://192.0.2.1:3671",
        "--database",
        path.to_str().expect("UTF-8 test path"),
    ]);
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(stderr.contains("WriterBusy"), "{stderr}");
    drop(writer);
    assert!(CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero")).is_ok());
}

#[tokio::test]
async fn serve_recovers_committed_history_after_process_termination() {
    struct CaptureChild(Child);

    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("service.sqlite");
    let database = path.to_str().expect("UTF-8 test path");

    for value in [41_u8, 42_u8] {
        let server = DeviceServer::start_at("127.0.0.1:0".parse().expect("loopback address"))
            .await
            .expect("start KNX tunnel server");
        let endpoint = format!("tunnel://{}", server.local_addr());
        let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
            .args(["serve", &endpoint, "--database", database])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("start capture process");
        let mut child = CaptureChild(child);
        let found = tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(status) = child.0.try_wait().expect("query capture process") {
                    let mut stderr = String::new();
                    if let Some(mut stream) = child.0.stderr.take() {
                        stream
                            .read_to_string(&mut stderr)
                            .expect("read error output");
                    }
                    panic!("capture process exited with {status}: {stderr}");
                }
                let frame = CemiFrame::new_l_data(
                    MessageCode::LDataInd,
                    IndividualAddress::from_raw(0x1101),
                    DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
                    Priority::Low,
                    &[0x00, 0x80, value],
                );
                server.send_frame(frame).await.expect("send loopback frame");
                if let Ok(history) = CaptureStore::open_existing(&path)
                    && history
                        .read_after(0, NonZeroU32::new(1_000).expect("nonzero"))
                        .expect("read committed history")
                        .iter()
                        .any(|capture| capture.event.frame().payload() == [0x00, 0x80, value])
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await;
        assert!(found.is_ok(), "capture process timed out on value {value}");
        child.0.kill().expect("terminate capture process");
        child.0.wait().expect("reap capture process");
        server.stop().await;
    }

    let history = CaptureStore::open_existing(&path).expect("reopen after second termination");
    let values: Vec<u8> = history
        .read_after(0, NonZeroU32::new(1_000).expect("nonzero"))
        .expect("read committed history")
        .into_iter()
        .map(|capture| capture.event.frame().payload()[2])
        .collect();
    assert!(values.contains(&41), "first process capture was lost");
    assert!(values.contains(&42), "second process capture was lost");
}

#[tokio::test]
async fn independent_capture_process_streams_committed_frames_over_local_ipc() {
    use std::fmt::Write as _;

    struct CaptureChild(Child);

    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("ipc.sqlite");
    let server = DeviceServer::start_at("127.0.0.1:0".parse().expect("loopback address"))
        .await
        .expect("start KNX tunnel server");
    let endpoint = format!("tunnel://{}", server.local_addr());
    let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args([
            "serve",
            &endpoint,
            "--database",
            path.to_str().expect("UTF-8 test path"),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start capture process");
    let mut child = CaptureChild(child);

    let received = tokio::time::timeout(Duration::from_secs(15), async {
        let mut client = loop {
            if let Ok(client) = IpcClient::connect(&path, true).await {
                break client;
            }
            assert!(
                child
                    .0
                    .try_wait()
                    .expect("capture process status")
                    .is_none(),
                "capture process exited before IPC became available"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        let status = devknx(&[
            "status",
            "--database",
            path.to_str().expect("UTF-8 test path"),
        ]);
        assert!(status.status.success(), "{:?}", status.stderr);
        let status_record: IpcMessage =
            serde_json::from_slice(&status.stdout).expect("one JSON status record");
        assert!(matches!(status_record, IpcMessage::State { .. }));
        loop {
            match client.next().await.expect("read local status") {
                Some(IpcMessage::State {
                    value: WireState::Connected { .. },
                }) => break,
                Some(_) => {}
                None => panic!("local IPC closed before connection"),
            }
        }

        let expected = response_frame();
        let original = expected.as_bytes().to_vec();
        server
            .send_frame(expected)
            .await
            .expect("send loopback frame");
        loop {
            match client.next().await.expect("read live capture") {
                Some(IpcMessage::Capture {
                    id: Some(id),
                    raw_cemi,
                    service,
                    ..
                }) => {
                    assert_eq!(id, 1);
                    assert_eq!(service, "Response");
                    let expected_hex = original.iter().fold(String::new(), |mut text, byte| {
                        write!(&mut text, "{byte:02x}").expect("write to String");
                        text
                    });
                    assert_eq!(raw_cemi, expected_hex);
                    break;
                }
                Some(_) => {}
                None => panic!("local IPC closed before capture"),
            }
        }
    })
    .await;
    assert!(received.is_ok(), "IPC capture timed out");
    child.0.kill().expect("terminate capture process");
    child.0.wait().expect("reap capture process");
    server.stop().await;
    assert_eq!(
        CaptureStore::open_existing(&path)
            .expect("replay after process death")
            .read_after(0, NonZeroU32::new(10).expect("nonzero"))
            .expect("read committed capture")
            .len(),
        1
    );
}

#[test]
fn history_and_export_read_the_same_committed_frame() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("captures.sqlite");
    let mut store = CaptureStore::open(&path, NonZeroU32::new(10).expect("nonzero"))
        .expect("create capture store");
    let frame = CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
        Priority::Low,
        &[0x00, 0x80, 0x01],
    );
    let event = CaptureEvent::received(
        CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().expect("socket address")),
        frame,
    );
    assert_eq!(store.insert(&event).expect("insert frame"), 1);
    let ets = EtsCatalog::from_bytes(
        br#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01"><GroupRange Name="Lighting"><GroupAddress Name="Hall light" Address="1/0/1" DPTs="DPST-1-1" /></GroupRange></GroupAddress-Export>"#,
        EtsFormat::GaXml01,
        CsvEncoding::Utf8,
    ).unwrap();
    store.import_ets(&ets).unwrap();
    drop(store);

    let path = path.to_str().expect("UTF-8 test path");
    let history = devknx(&["history", "--database", path, "--after", "0"]);
    assert!(history.status.success());
    let history = String::from_utf8(history.stdout).expect("UTF-8 history");
    assert!(history.contains("id=1 timestamp_ms="));
    assert!(history.contains("cemi=2900bce01101080102008001"));
    assert!(history.contains("ets_name=\"Hall light\""));

    let filtered = devknx(&["history", "--database", path, "--filter", "hall light"]);
    assert!(filtered.status.success());
    assert!(String::from_utf8_lossy(&filtered.stdout).contains("id=1"));
    let absent = devknx(&["history", "--database", path, "--filter", "kitchen"]);
    assert!(absent.status.success());
    assert!(absent.stdout.is_empty());

    let export = devknx(&["export", "--database", path]);
    assert!(export.status.success());
    let export = String::from_utf8(export.stdout).expect("UTF-8 CSV");
    assert_eq!(export.lines().count(), 2);
    assert!(export.contains(",GroupValueWrite,0x29,2900bce01101080102008001"));

    let empty = devknx(&["history", "--database", path, "--after", "1"]);
    assert!(empty.status.success());
    assert!(empty.stdout.is_empty());
}

#[test]
fn history_does_not_create_a_missing_database() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let path = directory.path().join("missing.sqlite");
    let output = devknx(&[
        "history",
        "--database",
        path.to_str().expect("UTF-8 test path"),
    ]);
    assert!(!output.status.success());
    assert!(!path.exists());
}

#[test]
fn router_loss_history_is_separate_and_survives_backup() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("captures.sqlite");
    let snapshot = directory.path().join("snapshot.sqlite");
    let mut writer = CaptureStore::open(&source, NonZeroU32::new(10).unwrap()).unwrap();
    writer
        .insert_routing_loss(&RoutingLossEvent::received(
            CaptureEndpoint::Router("224.0.23.12:3671".parse().unwrap()),
            RoutingLostMessage {
                source: "192.0.2.2:3671".parse().unwrap(),
                device_state: 3,
                lost_messages: 258,
            },
        ))
        .unwrap();
    let path = source.to_str().unwrap();
    let result = devknx(&["router-losses", "--database", path]);
    assert!(result.status.success(), "{:?}", result.stderr);
    let row: IpcMessage = serde_json::from_slice(result.stdout.trim_ascii()).unwrap();
    assert!(matches!(
        row,
        IpcMessage::RoutingLostMessage {
            id: Some(1),
            device_state: 3,
            lost_messages: 258,
            ..
        }
    ));
    assert!(devknx(&["history", "--database", path]).stdout.is_empty());
    assert!(
        devknx(&["router-losses", "--database", path, "--after", "1"])
            .stdout
            .is_empty()
    );
    let backup = devknx(&[
        "backup",
        "--database",
        path,
        "--output",
        snapshot.to_str().unwrap(),
    ]);
    assert!(backup.status.success(), "{:?}", backup.stderr);
    let restored = devknx(&["router-losses", "--database", snapshot.to_str().unwrap()]);
    assert!(restored.status.success());
    assert_eq!(restored.stdout, result.stdout);

    let missing = directory.path().join("missing.sqlite");
    let result = devknx(&["router-losses", "--database", missing.to_str().unwrap()]);
    assert!(!result.status.success());
    assert!(!missing.exists());
}

#[tokio::test]
async fn router_report_flows_through_service_ipc_and_history() {
    use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};

    struct CaptureChild(Child);
    impl Drop for CaptureChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    let probe = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("router.sqlite");
    let database = path.to_str().unwrap();
    let endpoint = format!("router://239.255.23.12:{port}");
    let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
        .args(["serve", &endpoint, "--database", database])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut child = CaptureChild(child);

    tokio::time::timeout(Duration::from_secs(15), async {
        let mut client = loop {
            if let Ok(client) = IpcClient::connect(&path, true).await {
                break client;
            }
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "capture process exited"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        loop {
            match client.next().await.unwrap() {
                Some(IpcMessage::State {
                    value: WireState::Connected { .. },
                }) => break,
                Some(_) => {}
                None => panic!("service closed before router connection"),
            }
        }
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        sender
            .send_to(
                &[0x06, 0x10, 0x05, 0x31, 0x00, 0x0a, 0x04, 0x03, 0x01, 0x02],
                SocketAddrV4::new(Ipv4Addr::LOCALHOST, port),
            )
            .unwrap();
        let live = loop {
            match client.next().await.unwrap() {
                Some(message @ IpcMessage::RoutingLostMessage { .. }) => break message,
                Some(_) => {}
                None => panic!("service closed before router diagnostic"),
            }
        };
        assert!(matches!(
            live,
            IpcMessage::RoutingLostMessage {
                id: Some(1),
                device_state: 3,
                lost_messages: 258,
                ..
            }
        ));
        let history = devknx(&["router-losses", "--database", database]);
        assert!(history.status.success(), "{:?}", history.stderr);
        let stored: IpcMessage = serde_json::from_slice(history.stdout.trim_ascii()).unwrap();
        assert_eq!(stored, live);
        assert!(
            devknx(&["history", "--database", database])
                .stdout
                .is_empty()
        );
    })
    .await
    .expect("router report reaches IPC and history");
}

#[test]
fn backup_is_restorable_and_never_overwrites_a_destination() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let source = directory.path().join("captures.sqlite");
    let snapshot = directory.path().join("snapshot.sqlite");
    let mut writer = CaptureStore::open(&source, NonZeroU32::new(10).expect("nonzero"))
        .expect("create capture store");
    let frame = CemiFrame::new_l_data(
        MessageCode::LDataInd,
        IndividualAddress::from_raw(0x1101),
        DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
        Priority::Low,
        &[0x00, 0x80, 0x01],
    );
    writer
        .insert(&CaptureEvent::received(
            CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().expect("socket address")),
            frame,
        ))
        .expect("commit first frame");
    let source_path = source.to_str().expect("UTF-8 path");
    let snapshot_path = snapshot.to_str().expect("UTF-8 path");
    let result = devknx(&[
        "backup",
        "--database",
        source_path,
        "--output",
        snapshot_path,
    ]);
    assert!(result.status.success(), "{:?}", result.stderr);
    let snapshot_store = CaptureStore::open_existing(&snapshot).expect("open snapshot");
    let restored = snapshot_store
        .read_after(0, NonZeroU32::new(10).expect("nonzero"))
        .expect("read snapshot");
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].event.frame().payload(), &[0x00, 0x80, 0x01]);
    drop(snapshot_store);

    let recovery_path = directory.path().join("recovered.sqlite");
    Connection::open(&recovery_path)
        .expect("create recovery database")
        .restore(MAIN_DB, &snapshot, None::<fn(rusqlite::backup::Progress)>)
        .expect("restore snapshot through SQLite");
    assert_eq!(
        CaptureStore::open_existing(&recovery_path)
            .expect("open recovered database")
            .read_after(0, NonZeroU32::new(10).expect("nonzero"))
            .expect("read recovered frame")[0]
            .event
            .frame()
            .payload(),
        &[0x00, 0x80, 0x01]
    );

    let snapshot_bytes = std::fs::read(&snapshot).expect("read snapshot bytes");

    let result = devknx(&[
        "backup",
        "--database",
        source_path,
        "--output",
        snapshot_path,
    ]);
    assert!(!result.status.success());
    assert_eq!(
        std::fs::read(&snapshot).expect("read preserved bytes"),
        snapshot_bytes
    );
    assert_eq!(
        CaptureStore::open_existing(&snapshot)
            .expect("original snapshot preserved")
            .read_after(0, NonZeroU32::new(10).expect("nonzero"))
            .expect("read preserved snapshot")
            .len(),
        1
    );
}

#[test]
fn version_matches_package() {
    let output = devknx(&["--version"]);
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout)
            .expect("UTF-8 version")
            .trim(),
        concat!("devknx ", env!("CARGO_PKG_VERSION"))
    );
}
