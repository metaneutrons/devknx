// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! The daemon stops by itself when idle, unless a KNX session or a lease
//! holds it, and a stopping daemon tells late clients to try again.

use std::io::Write as _;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use knx_rs_ip::DeviceServer;
use serde_json::Value;

struct Sandbox {
    directory: tempfile::TempDir,
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Sandbox {
    fn new() -> Self {
        Self {
            directory: tempfile::tempdir().expect("isolated test data directory"),
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_devknx"));
        command
            .args(args)
            .env("HOME", self.directory.path())
            .env("XDG_DATA_HOME", self.directory.path())
            .env("LOCALAPPDATA", self.directory.path())
            .env("DEVKNX_NO_VERSION_WARNING", "1")
            // What a daemon started by a client prints, for a failure to show.
            .env("DEVKNX_DAEMON_LOG", self.daemon_log_path())
            .stdin(Stdio::null());
        command
    }

    fn daemon_log_path(&self) -> std::path::PathBuf {
        self.directory.path().join("daemon.log")
    }

    /// What the daemons started by clients printed so far.
    fn daemon_log(&self) -> String {
        std::fs::read_to_string(self.daemon_log_path()).unwrap_or_default()
    }

    /// Start a command with its output going to files, not pipes: a daemon
    /// it starts may outlive it and would keep a pipe open, so reading to the
    /// end would wait for the daemon, not the command.
    fn start(&self, args: &[&str]) -> Started {
        let out = tempfile::tempfile().expect("a file for stdout");
        let err = tempfile::tempfile().expect("a file for stderr");
        let child = self
            .command(args)
            .stdout(out.try_clone().unwrap())
            .stderr(err.try_clone().unwrap())
            .spawn()
            .expect("run isolated devknx CLI");
        Started {
            child,
            out,
            err,
            label: args.join(" "),
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        self.start(args).finish()
    }

    fn json(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "devknx {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("JSON reply")
    }

    /// A foreground daemon with this idle period, ready to answer.
    fn start_daemon(&self, idle: &str) -> ChildGuard {
        let child = self
            .command(&["daemon", "--idle-timeout", idle])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the daemon");
        let mut child = ChildGuard(child);
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.json(&["daemon", "--status"])["running"] != true {
            assert!(child.0.try_wait().unwrap().is_none(), "the daemon exited");
            assert!(Instant::now() < deadline, "the daemon did not start");
            std::thread::sleep(Duration::from_millis(50));
        }
        child
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self
            .command(&["daemon", "--stop"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// A command that was started and has not been waited for.
struct Started {
    child: Child,
    out: std::fs::File,
    err: std::fs::File,
    label: String,
}

impl Started {
    /// Wait for the command, and give up on it after a minute.
    fn finish(mut self) -> Output {
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!(
                    "devknx {} did not finish in a minute; stderr so far: {}",
                    self.label,
                    read_all(self.err)
                );
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        Output {
            status,
            stdout: read_all(self.out).into_bytes(),
            stderr: read_all(self.err).into_bytes(),
        }
    }
}

fn read_all(mut file: std::fs::File) -> String {
    use std::io::{Read as _, Seek as _, SeekFrom};

    let mut text = String::new();
    file.seek(SeekFrom::Start(0)).unwrap();
    file.read_to_string(&mut text).unwrap();
    text
}

/// Whether the daemon process ends within `within`.
fn ends_within(child: &mut ChildGuard, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if child.0.try_wait().unwrap().is_some() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// An idle daemon stops after its period; a period of `0` never stops.
#[test]
fn an_idle_daemon_stops_by_itself() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start_daemon("2s");
    let status = sandbox.json(&["daemon", "--status"]);
    assert_eq!(status["lifecycle"]["idle_seconds"], 2, "{status}");
    assert_eq!(status["lifecycle"]["source"], "start_flag", "{status}");
    // Asking for the status is not activity, so it does not keep it running.
    assert!(ends_within(&mut daemon, Duration::from_secs(6)));
    assert_eq!(sandbox.json(&["daemon", "--status"])["running"], false);

    // Counter-probe: a period of `0` keeps the daemon running.
    let mut daemon = sandbox.start_daemon("0");
    assert!(!ends_within(&mut daemon, Duration::from_secs(4)));
}

/// The running daemon's period can be changed, and the change takes effect.
#[test]
fn the_idle_period_can_be_changed_on_the_running_daemon() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start_daemon("0");
    let changed = sandbox.json(&["daemon", "--set-idle-timeout", "1s"]);
    assert_eq!(changed["state"]["idle_seconds"], 1, "{changed}");
    assert_eq!(changed["state"]["source"], "runtime", "{changed}");
    assert!(ends_within(&mut daemon, Duration::from_secs(5)));

    // Without a daemon there is nothing to change, and it says so.
    let refused = sandbox.run(&["daemon", "--set-idle-timeout", "1s"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("not running"));
}

/// A configured KNX session holds the daemon; disconnecting it starts the
/// idle period.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_holds_the_daemon() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start_daemon("2s");
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    sandbox.json(&["connect", &endpoint]);
    assert!(!ends_within(&mut daemon, Duration::from_secs(4)));
    let status = sandbox.json(&["daemon", "--status"]);
    assert_eq!(status["lifecycle"]["sessions"], 1, "{status}");
    assert!(
        status["lifecycle"]["stops_in_seconds"].is_null(),
        "{status}"
    );

    sandbox.json(&["disconnect", &endpoint]);
    assert!(ends_within(&mut daemon, Duration::from_secs(6)));
}

/// An MCP server holds the daemon with a lease while it runs, and releases
/// it when it ends.
#[test]
fn an_mcp_server_holds_the_daemon_while_it_runs() {
    let sandbox = Sandbox::new();
    let mut daemon = sandbox.start_daemon("2s");
    let database = sandbox.directory.path().join("capture.sqlite");
    let mut mcp = sandbox
        .command(&["mcp", "--database", database.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start the MCP server");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = sandbox.json(&["daemon", "--status"]);
        if status["lifecycle"]["leases"] == serde_json::json!(["mcp"]) {
            break;
        }
        assert!(Instant::now() < deadline, "no MCP lease: {status}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ends_within(&mut daemon, Duration::from_secs(4)));

    // Ending the MCP session releases the lease, and the daemon goes idle.
    let mut stdin = mcp.stdin.take().unwrap();
    let _ = stdin.flush();
    drop(stdin);
    let _ = mcp.wait();
    assert!(ends_within(&mut daemon, Duration::from_secs(6)));
}

/// Requests that arrive while the daemon stops are not lost: each one is
/// either served or answered `Stopping`, and its client then starts a new
/// daemon and sends it again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_during_a_stop_reach_a_daemon() {
    let sandbox = Sandbox::new();
    let gateway = DeviceServer::start_at("127.0.0.1:0".parse().unwrap())
        .await
        .unwrap();
    let endpoint = format!("tunnel://{}", gateway.local_addr());
    for round in 0..3 {
        sandbox.json(&["connect", &endpoint]);
        let clients: Vec<_> = (0..6)
            .map(|_| sandbox.start(&["connect", &endpoint]))
            .collect();
        let stop = sandbox.run(&["daemon", "--stop"]);
        assert!(stop.status.success(), "round {round}: {stop:?}");
        for client in clients {
            let output = client.finish();
            assert!(
                output.status.success(),
                "round {round}: {:?}\nstdout: {}\nstderr: {}\ndaemons started by clients:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
                sandbox.daemon_log()
            );
            let reply: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(reply["result"], "session", "round {round}: {reply}");
        }
    }
}
