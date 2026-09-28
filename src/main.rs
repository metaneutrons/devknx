// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use clap::{Args, Parser, Subcommand, ValueEnum};
use devknx::api::{self, ApiConfig};
use devknx::capture::{CaptureEvent, RoutingLossEvent};
use devknx::control::{ControlClient, ControlRequest, ControlResponse};
use devknx::daemon;
use devknx::ets::{CsvEncoding, EtsCatalog, EtsFormat, parse_group_address};
use devknx::ipc::{IpcClient, IpcMessage, IpcServer, WireState};
use devknx::mcp;
use devknx::operations::{OperationRequest, RawPayload, prepare};
use devknx::paths;
use devknx::service::{CaptureService, LiveRoutingLoss, ReconnectPolicy};
use devknx::storage::CaptureStore;
use knx_rs_ip::{ConnectionSpec, discovery, parse_url};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;
use std::time::UNIX_EPOCH;
use tokio::sync::{broadcast, oneshot};

#[cfg(feature = "gui")]
mod gui;
#[cfg(any(feature = "gui", feature = "tui"))]
mod interface;
#[cfg(feature = "gui")]
mod platform;
#[cfg(feature = "tui")]
mod tui;

#[derive(Parser)]
#[command(name = "devknx", version, about = "Discover and monitor KNXnet/IP")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Clone, Copy, ValueEnum)]
enum EtsInputFormat {
    Csv,
    Xml,
}

impl From<EtsInputFormat> for EtsFormat {
    fn from(value: EtsInputFormat) -> Self {
        match value {
            EtsInputFormat::Csv => Self::Csv31,
            EtsInputFormat::Xml => Self::GaXml01,
        }
    }
}

#[derive(Args)]
#[group(id = "capture_selector", required = true, multiple = false)]
struct CaptureSelector {
    /// Select the capture associated with this KNXnet/IP endpoint.
    #[arg(long, value_name = "URL")]
    endpoint: Option<String>,
    /// Select this SQLite capture database.
    #[arg(long, value_name = "PATH")]
    database: Option<PathBuf>,
}

#[derive(Args)]
#[group(id = "optional_capture_selector", multiple = false)]
struct OptionalCaptureSelector {
    /// Select metadata from the capture associated with this endpoint.
    #[arg(long, value_name = "URL")]
    endpoint: Option<String>,
    /// Select metadata from this SQLite capture database.
    #[arg(long, value_name = "PATH")]
    database: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Discover KNXnet/IP gateways on the local network.
    Discover,
    /// Run the connection-independent daemon, or inspect/stop an existing daemon.
    Daemon {
        #[arg(long, conflicts_with = "stop")]
        status: bool,
        #[arg(long)]
        stop: bool,
    },
    /// Start one KNX session in the per-user daemon.
    Connect {
        endpoint: String,
        #[arg(long)]
        database: Option<PathBuf>,
        #[arg(long, default_value_t = NonZeroU32::new(100_000).expect("nonzero"))]
        max_events: NonZeroU32,
    },
    /// Stop one KNX session without stopping the daemon.
    Disconnect { endpoint: String },
    /// List the daemon's active KNX sessions without starting it.
    Sessions,
    /// Enable, disable, or inspect the daemon-owned REST listener.
    Rest {
        #[arg(long, conflicts_with_all = ["disable", "status"])]
        enable: bool,
        #[arg(long, conflicts_with = "status")]
        disable: bool,
        #[arg(long)]
        status: bool,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: SocketAddr,
        #[arg(long)]
        token_env: Option<String>,
        #[arg(long)]
        allow_remote_writes: bool,
    },
    /// Subscribe to a managed KNX session until interrupted; capture continues afterward.
    Monitor {
        /// Endpoint URL, for example `tunnel://192.0.2.1:3671` or `router://224.0.23.12:3671`.
        endpoint: String,
        /// SQLite database for received frames; defaults to the endpoint's private capture.
        #[arg(long)]
        database: Option<PathBuf>,
        /// Maximum rows retained in the database.
        #[arg(long, default_value_t = NonZeroU32::new(100_000).expect("nonzero"))]
        max_events: NonZeroU32,
    },
    /// Legacy single-connection foreground owner (use daemon + connect).
    #[command(hide = true)]
    Serve {
        /// KNXnet/IP tunnel or router endpoint URL.
        endpoint: String,
        /// SQLite database owned by this capture process; defaults to the endpoint's private capture.
        #[arg(long)]
        database: Option<PathBuf>,
        /// Maximum rows retained in the database.
        #[arg(long, default_value_t = NonZeroU32::new(100_000).expect("nonzero"))]
        max_events: NonZeroU32,
    },
    /// Legacy foreground REST listener (use rest --enable).
    #[command(hide = true)]
    Api {
        /// Existing database owned by `serve` for live operations.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Listener address; non-loopback requires a bearer token.
        #[arg(long, default_value = "127.0.0.1:8765")]
        bind: SocketAddr,
        /// Name of an environment variable containing a token of at least 32 bytes.
        #[arg(long)]
        token_env: Option<String>,
        /// Permit DPT-validated typed writes through a non-loopback listener.
        #[arg(long)]
        allow_remote_writes: bool,
    },
    /// Serve structured MCP tools over standard input/output.
    Mcp {
        /// Existing capture database; its KNX session must already be connected.
        #[command(flatten)]
        selector: CaptureSelector,
    },
    /// Read the selected capture session's current connection state.
    Status {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
    },
    /// Stream state changes and committed captures as JSON lines.
    Follow {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
    },
    /// Read captured telegrams after a monotonic event ID.
    History {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Exclusive cursor; zero reads from the beginning.
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum rows to print (1–1000).
        #[arg(long, default_value_t = NonZeroU32::new(100).expect("nonzero"))]
        limit: NonZeroU32,
        /// Case-insensitive text filter within the requested history page.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Read durable router-reported routing losses after a separate event ID.
    RouterLosses {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Exclusive router-loss cursor; zero reads from the beginning.
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum rows to print (1–1000).
        #[arg(long, default_value_t = NonZeroU32::new(100).expect("nonzero"))]
        limit: NonZeroU32,
    },
    /// Export captured telegrams as CSV to standard output.
    Export {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Exclusive cursor; zero exports from the beginning.
        #[arg(long, default_value_t = 0)]
        after: i64,
    },
    /// Save a consistent, non-overwriting SQLite capture snapshot.
    Backup {
        /// Existing SQLite capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// New snapshot file; an existing file is never replaced.
        #[arg(long)]
        output: PathBuf,
    },
    /// Transactionally replace ETS group-address metadata (disconnect the session first).
    EtsImport {
        /// Existing ETS CSV 3/1 or GA Export 01 XML file.
        file: PathBuf,
        /// Capture database to enrich, or a new database to create.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Export format; explicit to avoid guessing from a filename.
        #[arg(long, value_enum)]
        format: EtsInputFormat,
        /// Decode legacy CSV as ISO-8859-1 instead of strict UTF-8.
        #[arg(long)]
        latin1: bool,
    },
    /// Read active ETS metadata for one group address.
    EtsLookup {
        /// Existing capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Three-level, two-level, decimal, or ETS hexadecimal group address.
        address: String,
    },
    /// Preview exact DPT-encoded cEMI bytes without sending.
    WritePreview {
        #[command(flatten)]
        selector: OptionalCaptureSelector,
        /// Explicit DPT, required when ETS is absent or ambiguous.
        #[arg(long)]
        dpt: Option<String>,
        /// KNX group address.
        address: String,
        /// Typed value, for example `true`, `42`, or `21.5`.
        value: String,
    },
    /// Transmit a DPT-validated group value through the active KNX session.
    Write {
        /// Active session database or KNX endpoint to connect on demand.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Explicit DPT, required when ETS is absent or ambiguous.
        #[arg(long)]
        dpt: Option<String>,
        /// KNX group address.
        address: String,
        /// Typed value.
        value: String,
    },
    /// Send an explicit raw group value; expert use only, audited separately.
    WriteRaw {
        /// Active session database or KNX endpoint to connect on demand.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Inline APCI value, 0–63; exclusive with `--bytes`.
        #[arg(long, conflicts_with = "bytes")]
        inline: Option<u8>,
        /// Complete octets in hexadecimal; exclusive with `--inline`.
        #[arg(long, conflicts_with = "inline")]
        bytes: Option<String>,
        /// KNX group address.
        address: String,
    },
    /// Send a group read and report a matching response or no response.
    Read {
        /// Active session database or KNX endpoint to connect on demand.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Response deadline in milliseconds (1–30000).
        #[arg(long, default_value_t = 2_000)]
        timeout_ms: u32,
        /// KNX group address.
        address: String,
    },
    /// Read durable operation attempts, including raw/typed distinction.
    Audit {
        /// Existing capture database.
        #[command(flatten)]
        selector: CaptureSelector,
        /// Exclusive audit cursor.
        #[arg(long, default_value_t = 0)]
        after: i64,
        /// Maximum rows to print (1–1000).
        #[arg(long, default_value_t = NonZeroU32::new(100).expect("nonzero"))]
        limit: NonZeroU32,
    },
    /// Open the native desktop application.
    #[cfg(feature = "gui")]
    Gui {
        /// Existing capture database to attach on startup.
        #[arg(long)]
        database: Option<PathBuf>,
        /// Run a bounded native window and menu smoke test, then exit.
        #[arg(long, hide = true)]
        smoke: bool,
        /// Qualify live capture, scrolling and owner restart in a native window.
        #[arg(long, hide = true, requires = "database", conflicts_with = "smoke")]
        smoke_live: bool,
    },
    /// Open the interactive terminal monitor.
    #[cfg(feature = "tui")]
    Tui {
        #[command(flatten)]
        selector: CaptureSelector,
    },
}

#[tokio::main]
#[expect(
    clippy::too_many_lines,
    reason = "top-level CLI dispatch names each application command explicitly"
)]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Some(Command::Discover) => {
            let gateways = discovery::discover(Ipv4Addr::UNSPECIFIED).await?;
            for gateway in gateways {
                println!("{gateway:?}");
            }
        }
        Some(Command::Daemon { status, stop }) => {
            if status {
                match ControlClient::request_existing(ControlRequest::Ping).await {
                    Ok(ControlResponse::Pong) => println!("{{\"running\":true}}"),
                    Ok(response) => print_control_response(&response)?,
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::NotFound
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::AddrNotAvailable
                        ) =>
                    {
                        println!("{{\"running\":false}}");
                    }
                    Err(error) => return Err(error.into()),
                }
            } else if stop {
                print_control_response(
                    &ControlClient::request_existing(ControlRequest::Stop).await?,
                )?;
            } else {
                daemon::run()
                    .await
                    .map_err(|error| io::Error::other(error.to_string()))?;
            }
        }
        Some(Command::Connect {
            endpoint,
            database,
            max_events,
        }) => {
            let response = connect_session(&endpoint, database, max_events).await?;
            print_control_response(&response)?;
        }
        Some(Command::Disconnect { endpoint }) => {
            let endpoint = paths::canonical_endpoint(&endpoint).map_err(io::Error::other)?;
            print_control_response(
                &ControlClient::request_existing(ControlRequest::Disconnect { endpoint }).await?,
            )?;
        }
        Some(Command::Sessions) => {
            print_control_response(&ControlClient::request_existing(ControlRequest::List).await?)?;
        }
        Some(Command::Rest {
            enable,
            disable,
            status: _,
            endpoint,
            bind,
            token_env,
            allow_remote_writes,
        }) => {
            if !enable
                && (endpoint.is_some()
                    || token_env.is_some()
                    || allow_remote_writes
                    || bind != "127.0.0.1:8765".parse::<SocketAddr>()?)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "REST listener options require --enable",
                )
                .into());
            }
            let request = if enable {
                let endpoint = endpoint.ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "rest --enable requires --endpoint",
                    )
                })?;
                let endpoint = paths::canonical_endpoint(&endpoint).map_err(io::Error::other)?;
                let token = token_env
                    .map(|name| {
                        std::env::var(name).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "REST token environment variable is missing or not UTF-8",
                            )
                        })
                    })
                    .transpose()?;
                ControlRequest::RestEnable {
                    endpoint,
                    bind,
                    token,
                    allow_remote_writes,
                }
            } else if disable {
                ControlRequest::RestDisable
            } else {
                ControlRequest::RestStatus
            };
            print_control_response(&ControlClient::request_existing(request).await?)?;
        }
        Some(Command::Monitor {
            endpoint,
            database,
            max_events,
        }) => {
            let response = connect_session(&endpoint, database, max_events).await?;
            let database = match response {
                ControlResponse::Session { session } => session.database,
                ControlResponse::Error { reason } => return Err(io::Error::other(reason).into()),
                _ => return Err(io::Error::other("unexpected session response").into()),
            };
            run_monitor_client(&database).await?;
        }
        Some(Command::Serve {
            endpoint,
            database,
            max_events,
        }) => {
            let spec = parse_url(&endpoint)?;
            let database = resolve_endpoint_database(database, &endpoint)?;
            run_service(spec, database, max_events).await?;
        }
        Some(Command::Api {
            selector,
            bind,
            token_env,
            allow_remote_writes,
        }) => {
            let database = resolve_capture_selector(selector)?;
            let token = token_env
                .map(|name| {
                    std::env::var(name).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "REST token environment variable is missing or not UTF-8",
                        )
                    })
                })
                .transpose()?;
            api::run(ApiConfig {
                database,
                bind,
                token,
                allow_remote_writes,
            })
            .await?;
        }
        Some(Command::Mcp { selector }) => mcp::run(resolve_capture_selector(selector)?).await?,
        Some(Command::Status { selector }) => {
            run_ipc_client(&resolve_capture_selector(selector)?, false).await?;
        }
        Some(Command::Follow { selector }) => {
            run_ipc_client(&resolve_capture_selector(selector)?, true).await?;
        }
        Some(Command::History {
            selector,
            after,
            limit,
            filter,
        }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            for capture in store.read_after(after, limit)? {
                let mut line = format_event(&capture.event);
                if let knx_rs_core::address::DestinationAddress::Group(address) =
                    capture.event.frame().destination_address()
                    && let Some(group) = store.ets_group(address)?
                {
                    write!(line, " ets_name={:?} dpts={:?}", group.name, group.dpts)?;
                }
                if filter
                    .as_ref()
                    .is_none_or(|query| line.to_lowercase().contains(&query.to_lowercase()))
                {
                    writeln!(output, "id={} {line}", capture.id)?;
                }
            }
        }
        Some(Command::RouterLosses {
            selector,
            after,
            limit,
        }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            for row in store.read_routing_losses_after(after, limit)? {
                let message = IpcMessage::from(&LiveRoutingLoss {
                    id: Some(row.id),
                    event: row.event,
                });
                writeln!(output, "{}", serde_json::to_string(&message)?)?;
            }
        }
        Some(Command::Export { selector, after }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            store.export_csv(&mut output, after)?;
        }
        Some(Command::Backup { selector, output }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            store.backup_to(&output)?;
        }
        Some(Command::EtsImport {
            file,
            selector,
            format,
            latin1,
        }) => {
            let database = resolve_capture_selector(selector)?;
            let encoding = if latin1 {
                CsvEncoding::Latin1
            } else {
                CsvEncoding::Utf8
            };
            let catalog = EtsCatalog::from_file(&file, format.into(), encoding)?;
            let groups = catalog.len();
            let mut store = CaptureStore::open_for_ets_import(&database)?;
            let revision = store.import_ets(&catalog)?;
            println!("revision={revision} groups={groups}");
        }
        Some(Command::EtsLookup { selector, address }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            let address = parse_group_address(&address)?;
            let result = serde_json::json!({
                "revision": store.ets_revision()?,
                "address_raw": address.raw(),
                "group": store.ets_group(address)?,
            });
            println!("{}", serde_json::to_string(&result)?);
        }
        Some(Command::WritePreview {
            selector,
            dpt,
            address,
            value,
        }) => {
            let address_raw = parse_group_address(&address)?.raw();
            let database = resolve_optional_capture_selector(selector)?;
            let group = match database {
                Some(database) => CaptureStore::open_existing(&database)?
                    .ets_group(knx_rs_core::address::GroupAddress::from_raw(address_raw))?,
                None => None,
            };
            let prepared = prepare(
                OperationRequest::TypedWrite {
                    address_raw,
                    dpt,
                    value,
                },
                group.as_ref(),
            )?;
            let result = serde_json::json!({
                "address_raw": address_raw,
                "dpt": prepared.dpt.map(|dpt| dpt.to_string()),
                "raw_cemi": to_hex(prepared.frame.as_bytes()),
                "transmitted": false,
            });
            println!("{}", serde_json::to_string(&result)?);
        }
        Some(Command::Write {
            selector,
            dpt,
            address,
            value,
        }) => {
            let database = resolve_live_selector(selector).await?;
            let address_raw = parse_group_address(&address)?.raw();
            execute_remote_operation(
                &database,
                OperationRequest::TypedWrite {
                    address_raw,
                    dpt,
                    value,
                },
            )
            .await?;
        }
        Some(Command::WriteRaw {
            selector,
            inline,
            bytes,
            address,
        }) => {
            let database = resolve_live_selector(selector).await?;
            let address_raw = parse_group_address(&address)?.raw();
            let payload = match (inline, bytes) {
                (Some(value), None) => RawPayload::Inline(value),
                (None, Some(bytes)) => RawPayload::Bytes(parse_hex_bytes(&bytes)?),
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "specify --inline or --bytes",
                    )
                    .into());
                }
            };
            execute_remote_operation(
                &database,
                OperationRequest::RawWrite {
                    address_raw,
                    payload,
                },
            )
            .await?;
        }
        Some(Command::Read {
            selector,
            timeout_ms,
            address,
        }) => {
            let database = resolve_live_selector(selector).await?;
            let address_raw = parse_group_address(&address)?.raw();
            execute_remote_operation(
                &database,
                OperationRequest::Read {
                    address_raw,
                    timeout_ms,
                },
            )
            .await?;
        }
        Some(Command::Audit {
            selector,
            after,
            limit,
        }) => {
            let database = resolve_capture_selector(selector)?;
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            for entry in store.read_operation_audit_after(after, limit)? {
                writeln!(output, "{}", serde_json::to_string(&entry)?)?;
            }
        }
        #[cfg(feature = "gui")]
        Some(Command::Gui {
            database,
            smoke,
            smoke_live,
        }) => gui::run(database, smoke, smoke_live)?,
        #[cfg(feature = "tui")]
        Some(Command::Tui { selector }) => {
            let fixed_database = selector.database.is_some();
            let endpoint = selector.endpoint.clone();
            let database = resolve_capture_selector(selector)?;
            tui::run(database, fixed_database, endpoint.as_deref())?;
        }
        #[cfg(feature = "gui")]
        None => gui::run(None, false, false)?,
        #[cfg(not(feature = "gui"))]
        None => {
            eprintln!("No command specified. Use --help for available commands.");
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn execute_remote_operation(
    database: &std::path::Path,
    request: OperationRequest,
) -> Result<(), Box<dyn std::error::Error>> {
    let result = IpcClient::operate(database, &request).await?;
    match result {
        IpcMessage::OperationResult { .. } => {
            println!("{}", serde_json::to_string(&result)?);
            Ok(())
        }
        IpcMessage::OperationError { reason } => Err(io::Error::other(reason).into()),
        _ => Err(io::Error::other("unexpected operation response").into()),
    }
}

async fn connect_session(
    endpoint: &str,
    database: Option<PathBuf>,
    max_events: NonZeroU32,
) -> io::Result<ControlResponse> {
    let endpoint = paths::canonical_endpoint(endpoint).map_err(io::Error::other)?;
    ControlClient::ensure_daemon().await?;
    ControlClient::request_existing(ControlRequest::Connect {
        endpoint,
        database,
        max_events: max_events.get(),
    })
    .await
}

async fn resolve_live_selector(selector: CaptureSelector) -> io::Result<PathBuf> {
    match (selector.endpoint, selector.database) {
        (Some(endpoint), None) => {
            match connect_session(&endpoint, None, NonZeroU32::new(100_000).expect("nonzero"))
                .await?
            {
                ControlResponse::Session { session } => {
                    wait_connected(&session.database).await?;
                    Ok(session.database)
                }
                ControlResponse::Error { reason } => Err(io::Error::other(reason)),
                _ => Err(io::Error::other("unexpected session response")),
            }
        }
        (None, Some(database)) => Ok(database),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "select exactly one of --endpoint or --database",
        )),
    }
}

async fn wait_connected(database: &std::path::Path) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut client = IpcClient::connect(database, true)
            .await
            .map_err(io::Error::other)?;
        loop {
            match client.next().await.map_err(io::Error::other)? {
                Some(IpcMessage::State {
                    value: WireState::Connected { .. },
                    ..
                }) => return Ok(()),
                Some(IpcMessage::State {
                    value: WireState::StorageFailed { reason },
                    ..
                }) => return Err(io::Error::other(reason)),
                Some(_) => {}
                None => return Err(io::Error::other("KNX session closed while connecting")),
            }
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "KNX session did not connect within 15 seconds",
        )
    })?
}

fn print_control_response(response: &ControlResponse) -> Result<(), Box<dyn std::error::Error>> {
    if let ControlResponse::Error { reason } = response {
        return Err(io::Error::other(reason.clone()).into());
    }
    println!("{}", serde_json::to_string(&response)?);
    Ok(())
}

fn parse_hex_bytes(value: &str) -> Result<Vec<u8>, io::Error> {
    let value = value.trim();
    if value.is_empty() || !value.len().is_multiple_of(2) || value.len() > 64 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "raw bytes need 1–32 hexadecimal octets",
        ));
    }
    value
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "non-ASCII hex"))?;
            u8::from_str_radix(pair, 16)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid hex byte"))
        })
        .collect()
}

fn to_hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(output, "{byte:02x}").expect("writing to String cannot fail");
    }
    output
}

async fn run_service(
    spec: ConnectionSpec,
    database: PathBuf,
    max_events: NonZeroU32,
) -> Result<(), Box<dyn std::error::Error>> {
    let configured_endpoint = match &spec {
        ConnectionSpec::Tunnel(address) => format!("tunnel://{address}"),
        ConnectionSpec::Router(address) => format!("router://{address}"),
    };
    let store = CaptureStore::open_bound(&database, max_events, &configured_endpoint)?;
    let queue_capacity = NonZeroUsize::new(1_024).expect("nonzero live queue capacity");
    let service = CaptureService::new(
        spec,
        Some(store),
        ReconnectPolicy::default(),
        queue_capacity,
    );
    let mut states = service.subscribe_state();
    let mut routing_losses = service.subscribe_routing_losses();
    let ipc_states = service.subscribe_state();
    let ipc_frames = service.subscribe_frames();
    let ipc_routing_losses = service.subscribe_routing_losses();
    let ipc_operations = service.operation_sender();
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::mpsc::channel(1);
    let ipc = IpcServer::bind(&database)?
        .with_shutdown(shutdown_tx)
        .with_configured_endpoint(configured_endpoint);
    let mut ipc_task =
        tokio::spawn(ipc.run(ipc_states, ipc_frames, ipc_routing_losses, ipc_operations));
    let (stop_tx, stop_rx) = oneshot::channel();
    let mut task = tokio::spawn(async move {
        let mut service = service;
        service
            .run_until(async {
                let _ = stop_rx.await;
            })
            .await
    });
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let signal_error = loop {
        tokio::select! {
            biased;
            result = &mut signal => break result.err(),
            _ = shutdown_rx.recv() => break None,
            result = &mut task => {
                ipc_task.abort();
                return Ok(result??);
            }
            result = &mut ipc_task => {
                task.abort();
                return Err(match result {
                    Ok(Err(error)) => Box::new(error) as Box<dyn std::error::Error>,
                    Ok(Ok(())) => Box::new(io::Error::other("local IPC server stopped")),
                    Err(error) => Box::new(error),
                });
            }
            changed = states.changed() => {
                if changed.is_ok() {
                    eprintln!("connection_state={:?}", *states.borrow_and_update());
                }
            }
            report = routing_losses.recv() => {
                match report {
                    Ok(live) => eprintln!("{}", format_routing_loss(&live.event, live.id)),
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        eprintln!("local_router_loss_subscriber_lagged={count}");
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
                }
            }
        }
    };
    let _ = stop_tx.send(());
    task.await??;
    ipc_task.abort();
    if let Some(error) = signal_error {
        return Err(Box::new(error));
    }
    Ok(())
}

async fn run_ipc_client(
    database: &std::path::Path,
    follow: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = IpcClient::connect(database, follow).await?;
    while let Some(message) = client.next().await? {
        let stdout = io::stdout();
        let mut output = stdout.lock();
        writeln!(output, "{}", serde_json::to_string(&message)?)?;
        output.flush()?;
        if !follow {
            break;
        }
    }
    Ok(())
}

fn resolve_endpoint_database(database: Option<PathBuf>, endpoint: &str) -> io::Result<PathBuf> {
    database.map_or_else(|| endpoint_database(endpoint), Ok)
}

fn resolve_capture_selector(selector: CaptureSelector) -> io::Result<PathBuf> {
    match (selector.endpoint, selector.database) {
        (Some(endpoint), None) => endpoint_database(&endpoint),
        (None, Some(database)) => Ok(database),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "select exactly one of --endpoint or --database",
        )),
    }
}

fn resolve_optional_capture_selector(
    selector: OptionalCaptureSelector,
) -> io::Result<Option<PathBuf>> {
    match (selector.endpoint, selector.database) {
        (Some(endpoint), None) => endpoint_database(&endpoint).map(Some),
        (None, Some(database)) => Ok(Some(database)),
        (None, None) => Ok(None),
        (Some(_), Some(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "select at most one of --endpoint or --database",
        )),
    }
}

fn endpoint_database(endpoint: &str) -> io::Result<PathBuf> {
    paths::database_for_endpoint(endpoint)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))
}

async fn run_monitor_client(database: &std::path::Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut client = IpcClient::connect(database, true).await?;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return Ok(()),
            message = client.next() => match message? {
                Some(IpcMessage::Capture {
                    id, observed_at_ms, endpoint, direction, source,
                    destination, service, raw_cemi,
                }) => {
                    println!(
                        "id={id:?} timestamp_ms={observed_at_ms} endpoint={endpoint} direction={direction} source={source} destination={destination} service={service} cemi={raw_cemi}"
                    );
                    io::stdout().flush()?;
                }
                Some(message) => eprintln!("{}", serde_json::to_string(&message)?),
                None => return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "managed KNX session closed",
                ).into()),
            }
        }
    }
}
fn format_event(event: &CaptureEvent) -> String {
    let timestamp_ms = event
        .observed_at()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let frame = event.frame();
    let mut raw_hex = String::with_capacity(frame.as_bytes().len() * 2);
    for byte in frame.as_bytes() {
        write!(raw_hex, "{byte:02x}").expect("writing to String cannot fail");
    }
    format!(
        "timestamp_ms={timestamp_ms} endpoint={} direction={} source={} destination={} service={:?} message_code=0x{:02x} cemi={raw_hex}",
        event.endpoint(),
        event.direction(),
        frame.source_address(),
        frame.destination_address(),
        event.group_service(),
        frame.message_code_raw(),
    )
}

fn format_routing_loss(event: &RoutingLossEvent, id: Option<i64>) -> String {
    let timestamp_ms = event
        .observed_at()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let report = event.report();
    format!(
        "routing_lost_message id={id:?} timestamp_ms={timestamp_ms} endpoint={} source={} device_state={} lost_routing_frames={}",
        event.endpoint(),
        report.source,
        report.device_state,
        report.lost_messages,
    )
}

#[cfg(test)]
mod tests {
    use devknx::capture::CaptureEndpoint;
    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;

    use super::*;

    #[test]
    fn monitor_line_includes_context_and_exact_cemi() {
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x80, 0x01],
        );
        let event = CaptureEvent::received(
            CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap()),
            frame,
        );
        let line = format_event(&event);

        assert!(line.contains("endpoint=tunnel://192.0.2.1:3671"));
        assert!(line.contains("direction=received source=1.1.1 destination=1/0/1"));
        assert!(line.contains("service=Write message_code=0x29"));
        assert!(line.ends_with("cemi=2900bce01101080102008001"));
    }
}
