// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

#[cfg(feature = "gui")]
mod gui_live {
    use std::io::{BufRead, BufReader};
    use std::num::NonZeroU32;
    use std::path::Path;
    use std::process::{Child, Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::Duration;

    use devknx::ipc::{IpcClient, IpcMessage, WireState};
    use devknx::storage::{CaptureStore, MAX_PAGE_SIZE};
    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;
    use knx_rs_ip::DeviceServer;
    use tokio::sync::mpsc::error::TryRecvError;
    use tokio::sync::mpsc::{self, UnboundedReceiver};
    use tokio::time::{sleep, timeout};

    const OWNER_TIMEOUT: Duration = Duration::from_secs(15);
    const STAGE_TIMEOUT: Duration = Duration::from_secs(30);
    const GUI_EXIT_TIMEOUT: Duration = Duration::from_secs(15);
    const FRAME_INTERVAL: Duration = Duration::from_millis(25);

    struct ChildGuard(Child);

    impl ChildGuard {
        fn start(args: &[&str], stderr: Stdio) -> Self {
            let child = Command::new(env!("CARGO_BIN_EXE_devknx"))
                .args(args)
                .stdout(Stdio::null())
                .stderr(stderr)
                .spawn()
                .expect("start devknx child process");
            Self(child)
        }

        fn try_wait(&mut self) -> Option<ExitStatus> {
            self.0.try_wait().expect("query child process")
        }

        fn kill_and_wait(&mut self) {
            if self.try_wait().is_none() {
                self.0.kill().expect("terminate child process");
            }
            self.0.wait().expect("reap child process");
        }
    }

    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn group_frame(value: u8) -> CemiFrame {
        CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x80, value],
        )
    }

    async fn wait_for_connected_owner(
        owner: &mut ChildGuard,
        database: &Path,
        server: &DeviceServer,
        payload: u8,
    ) {
        timeout(OWNER_TIMEOUT, async {
            loop {
                if let Some(status) = owner.try_wait() {
                    panic!("capture owner exited before connecting: {status}");
                }
                if let Ok(mut client) = IpcClient::connect(database, false).await
                    && matches!(
                        timeout(Duration::from_secs(1), client.next()).await,
                        Ok(Ok(Some(IpcMessage::State {
                            value: WireState::Connected { .. }
                        })))
                    )
                {
                    return;
                }
                let _ = server.send_frame(group_frame(payload)).await;
                sleep(FRAME_INTERVAL).await;
            }
        })
        .await
        .expect("capture owner did not connect to the loopback tunnel");
    }

    async fn wait_for_stage(
        stages: &mut UnboundedReceiver<String>,
        gui: &mut ChildGuard,
        server: &DeviceServer,
        expected: &str,
        payload: u8,
    ) {
        timeout(STAGE_TIMEOUT, async {
            loop {
                match stages.try_recv() {
                    Ok(line) => {
                        assert_eq!(line, expected, "GUI live-smoke stage order");
                        return;
                    }
                    Err(TryRecvError::Disconnected) => {
                        panic!("GUI stderr closed before {expected}");
                    }
                    Err(TryRecvError::Empty) => {}
                }
                if let Some(status) = gui.try_wait() {
                    panic!("GUI exited before {expected}: {status}");
                }
                let _ = server.send_frame(group_frame(payload)).await;
                sleep(FRAME_INTERVAL).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for GUI stage {expected}"));
    }

    async fn wait_for_gui_exit(gui: &mut ChildGuard, server: &DeviceServer) -> ExitStatus {
        timeout(GUI_EXIT_TIMEOUT, async {
            loop {
                if let Some(status) = gui.try_wait() {
                    return status;
                }
                let _ = server.send_frame(group_frame(0x43)).await;
                sleep(FRAME_INTERVAL).await;
            }
        })
        .await
        .expect("GUI did not exit after recovery")
    }

    #[tokio::test]
    #[ignore = "requires a native display; run explicitly in CI GUI step"]
    #[expect(
        clippy::too_many_lines,
        reason = "the end-to-end test keeps owner, GUI, and durable-history assertions together"
    )]
    async fn native_gui_streams_disconnects_and_recovers_across_owner_restart() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("gui-live.sqlite");
        let database_arg = database.to_str().expect("UTF-8 test path");
        let server = DeviceServer::start_at("127.0.0.1:0".parse().expect("loopback address"))
            .await
            .expect("start loopback KNX tunnel server");
        let endpoint = format!("tunnel://{}", server.local_addr());

        let mut first_owner = ChildGuard::start(
            &["serve", &endpoint, "--database", database_arg],
            Stdio::null(),
        );
        wait_for_connected_owner(&mut first_owner, &database, &server, 0x40).await;

        let mut gui = ChildGuard::start(
            &["gui", "--database", database_arg, "--smoke-live"],
            Stdio::piped(),
        );
        let stderr = gui.0.stderr.take().expect("GUI stderr is piped");
        let (stage_sender, mut stage_receiver) = mpsc::unbounded_channel();
        let stage_reader = thread::Builder::new()
            .name("devknx-gui-live-stages".to_owned())
            .spawn(move || {
                for line in BufReader::new(stderr).lines() {
                    let Ok(line) = line else { break };
                    if line.starts_with("GUI live smoke: ") {
                        if stage_sender.send(line).is_err() {
                            break;
                        }
                    } else {
                        eprintln!("GUI: {line}");
                    }
                }
            })
            .expect("start GUI stage reader");

        wait_for_stage(
            &mut stage_receiver,
            &mut gui,
            &server,
            "GUI live smoke: streaming",
            0x41,
        )
        .await;

        first_owner.kill_and_wait();
        wait_for_stage(
            &mut stage_receiver,
            &mut gui,
            &server,
            "GUI live smoke: disconnected",
            0x42,
        )
        .await;

        server.stop().await;
        let restarted_server =
            DeviceServer::start_at("127.0.0.1:0".parse().expect("loopback address"))
                .await
                .expect("restart loopback KNX tunnel server");
        let restarted_endpoint = format!("tunnel://{}", restarted_server.local_addr());
        let mut restarted_owner = ChildGuard::start(
            &["serve", &restarted_endpoint, "--database", database_arg],
            Stdio::null(),
        );
        wait_for_connected_owner(&mut restarted_owner, &database, &restarted_server, 0x43).await;
        wait_for_stage(
            &mut stage_receiver,
            &mut gui,
            &restarted_server,
            "GUI live smoke: recovered",
            0x43,
        )
        .await;

        let gui_status = wait_for_gui_exit(&mut gui, &restarted_server).await;
        assert!(gui_status.success(), "GUI live smoke failed: {gui_status}");
        stage_reader.join().expect("join GUI stage reader");
        assert!(
            matches!(
                stage_receiver.try_recv(),
                Err(TryRecvError::Empty | TryRecvError::Disconnected)
            ),
            "GUI emitted an unexpected extra live-smoke stage"
        );

        restarted_owner.kill_and_wait();
        let history = CaptureStore::open_existing(&database).expect("open persisted captures");
        let page_size = NonZeroU32::new(MAX_PAGE_SIZE).expect("nonzero page size");
        let mut captures = Vec::new();
        let mut cursor = 0;
        loop {
            let page = history
                .read_after(cursor, page_size)
                .expect("read persisted capture history");
            if page.is_empty() {
                break;
            }
            cursor = page.last().expect("nonempty capture page").id;
            captures.extend(page);
        }
        assert!(!captures.is_empty(), "no loopback frames were captured");
        assert!(
            captures.windows(2).all(|pair| pair[0].id < pair[1].id),
            "capture IDs must stay strictly increasing across owner restart"
        );
        let latest_streaming_id = captures
            .iter()
            .filter(|capture| capture.event.frame().payload() == [0x00, 0x80, 0x41])
            .map(|capture| capture.id)
            .max()
            .expect("streaming-stage loopback frames persisted");
        let earliest_recovered_id = captures
            .iter()
            .filter(|capture| capture.event.frame().payload() == [0x00, 0x80, 0x43])
            .map(|capture| capture.id)
            .min()
            .expect("recovered-stage loopback frames persisted");
        assert!(
            earliest_recovered_id > latest_streaming_id,
            "capture IDs must continue increasing after owner restart"
        );

        restarted_server.stop().await;
    }
}
