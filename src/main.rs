// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use clap::{Parser, Subcommand, ValueEnum};
use devknx::api::{self, ApiConfig};
use devknx::capture::{CaptureEvent, RoutingLossEvent};
use devknx::ets::{CsvEncoding, EtsCatalog, EtsFormat, parse_group_address};
use devknx::ipc::{IpcClient, IpcMessage, IpcServer};
use devknx::mcp;
use devknx::operations::{OperationRequest, RawPayload, prepare};
use devknx::service::{CaptureService, LiveRoutingLoss, ReconnectPolicy};
use devknx::storage::CaptureStore;
use knx_rs_ip::{ConnectionSpec, discovery, parse_url};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::{Ipv4Addr, SocketAddr};
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
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

#[derive(Subcommand)]
enum Command {
    /// Discover KNXnet/IP gateways on the local network.
    Discover,
    /// Print received KNXnet/IP telegrams until interrupted.
    Monitor {
        /// Endpoint URL, for example `tunnel://192.0.2.1:3671` or `router://224.0.23.12:3671`.
        endpoint: String,
        /// Persist received frames to this SQLite database.
        #[arg(long)]
        database: Option<PathBuf>,
        /// Maximum rows retained in the database.
        #[arg(long, default_value_t = NonZeroU32::new(100_000).expect("nonzero"))]
        max_events: NonZeroU32,
    },
    /// Run persistent capture in the foreground, independently of a monitor client.
    Serve {
        /// KNXnet/IP tunnel or router endpoint URL.
        endpoint: String,
        /// SQLite database owned by this capture process.
        #[arg(long)]
        database: PathBuf,
        /// Maximum rows retained in the database.
        #[arg(long, default_value_t = NonZeroU32::new(100_000).expect("nonzero"))]
        max_events: NonZeroU32,
    },
    /// Serve the versioned REST API (off unless explicitly started).
    Api {
        /// Existing database owned by `serve` for live operations.
        #[arg(long)]
        database: PathBuf,
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
        /// Existing database owned by `serve` for live operations.
        #[arg(long)]
        database: PathBuf,
    },
    /// Read the current state of an independent capture process.
    Status {
        /// Existing SQLite database owned by `serve`.
        #[arg(long)]
        database: PathBuf,
    },
    /// Stream state changes and committed captures from `serve` as JSON lines.
    Follow {
        /// Existing SQLite database owned by `serve`.
        #[arg(long)]
        database: PathBuf,
    },
    /// Read captured telegrams after a monotonic event ID.
    History {
        /// Existing SQLite capture database.
        #[arg(long)]
        database: PathBuf,
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
        #[arg(long)]
        database: PathBuf,
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
        #[arg(long)]
        database: PathBuf,
        /// Exclusive cursor; zero exports from the beginning.
        #[arg(long, default_value_t = 0)]
        after: i64,
    },
    /// Save a consistent, non-overwriting SQLite capture snapshot.
    Backup {
        /// Existing SQLite capture database.
        #[arg(long)]
        database: PathBuf,
        /// New snapshot file; an existing file is never replaced.
        #[arg(long)]
        output: PathBuf,
    },
    /// Transactionally replace ETS group-address metadata (stop `serve` first).
    EtsImport {
        /// Existing ETS CSV 3/1 or GA Export 01 XML file.
        file: PathBuf,
        /// Capture database to enrich, or a new database to create.
        #[arg(long)]
        database: PathBuf,
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
        #[arg(long)]
        database: PathBuf,
        /// Three-level, two-level, decimal, or ETS hexadecimal group address.
        address: String,
    },
    /// Preview exact DPT-encoded cEMI bytes without sending.
    WritePreview {
        /// Existing capture database for ETS DPT declarations.
        #[arg(long)]
        database: Option<PathBuf>,
        /// Explicit DPT, required when ETS is absent or ambiguous.
        #[arg(long)]
        dpt: Option<String>,
        /// KNX group address.
        address: String,
        /// Typed value, for example `true`, `42`, or `21.5`.
        value: String,
    },
    /// Transmit a DPT-validated group value through the active `serve` owner.
    Write {
        /// Database owned by the active capture process.
        #[arg(long)]
        database: PathBuf,
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
        /// Database owned by the active capture process.
        #[arg(long)]
        database: PathBuf,
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
        /// Database owned by the active capture process.
        #[arg(long)]
        database: PathBuf,
        /// Response deadline in milliseconds (1–30000).
        #[arg(long, default_value_t = 2_000)]
        timeout_ms: u32,
        /// KNX group address.
        address: String,
    },
    /// Read durable operation attempts, including raw/typed distinction.
    Audit {
        /// Existing capture database.
        #[arg(long)]
        database: PathBuf,
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
    /// Open the interactive terminal monitor for an existing capture database.
    #[cfg(feature = "tui")]
    Tui {
        /// Existing capture database owned by `serve` while live.
        #[arg(long)]
        database: PathBuf,
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
        Some(Command::Monitor {
            endpoint,
            database,
            max_events,
        }) => {
            let spec = parse_url(&endpoint)?;
            run_monitor(spec, database, max_events).await?;
        }
        Some(Command::Serve {
            endpoint,
            database,
            max_events,
        }) => {
            let spec = parse_url(&endpoint)?;
            run_service(spec, database, max_events).await?;
        }
        Some(Command::Api {
            database,
            bind,
            token_env,
            allow_remote_writes,
        }) => {
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
        Some(Command::Mcp { database }) => mcp::run(database).await?,
        Some(Command::Status { database }) => run_ipc_client(&database, false).await?,
        Some(Command::Follow { database }) => run_ipc_client(&database, true).await?,
        Some(Command::History {
            database,
            after,
            limit,
            filter,
        }) => {
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
            database,
            after,
            limit,
        }) => {
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
        Some(Command::Export { database, after }) => {
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            store.export_csv(&mut output, after)?;
        }
        Some(Command::Backup { database, output }) => {
            let store = CaptureStore::open_existing(&database)?;
            store.backup_to(&output)?;
        }
        Some(Command::EtsImport {
            file,
            database,
            format,
            latin1,
        }) => {
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
        Some(Command::EtsLookup { database, address }) => {
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
            database,
            dpt,
            address,
            value,
        }) => {
            let address_raw = parse_group_address(&address)?.raw();
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
            database,
            dpt,
            address,
            value,
        }) => {
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
            database,
            inline,
            bytes,
            address,
        }) => {
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
            database,
            timeout_ms,
            address,
        }) => {
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
            database,
            after,
            limit,
        }) => {
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
        Some(Command::Tui { database }) => tui::run(database)?,
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
    let store = CaptureStore::open(&database, max_events)?;
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
    let ipc = IpcServer::bind(&database)?;
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

async fn run_monitor(
    spec: ConnectionSpec,
    database: Option<PathBuf>,
    max_events: NonZeroU32,
) -> Result<(), Box<dyn std::error::Error>> {
    let has_database = database.is_some();
    let store = database
        .as_deref()
        .map(|path| CaptureStore::open(path, max_events))
        .transpose()?;
    let queue_capacity = NonZeroUsize::new(1_024).expect("nonzero live queue capacity");
    let service = CaptureService::new(spec, store, ReconnectPolicy::default(), queue_capacity);
    let mut states = service.subscribe_state();
    let mut frames = service.subscribe_frames();
    let mut routing_losses = service.subscribe_routing_losses();
    let (stop_tx, stop_rx) = oneshot::channel();
    let mut stop_tx = Some(stop_tx);
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
    let mut terminal_error: Option<io::Error> = None;
    loop {
        tokio::select! {
            biased;
            result = &mut signal => {
                if let Err(error) = result {
                    terminal_error = Some(error);
                }
                break;
            }
            result = &mut task => return Ok(result??),
            changed = states.changed() => {
                if changed.is_ok() {
                    eprintln!("connection_state={:?}", *states.borrow_and_update());
                }
            }
            frame = frames.recv() => {
                match frame {
                    Ok(live) => {
                        let event_line = format_event(&live.event);
                        let result = {
                            let stdout = io::stdout();
                            let mut output = stdout.lock();
                            match live.id {
                                Some(id) => writeln!(output, "id={id} {event_line}"),
                                None => writeln!(output, "{event_line}"),
                            }.and_then(|()| output.flush())
                        };
                        if let Err(error) = result {
                            terminal_error = Some(error);
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(count)) => {
                        eprintln!("live_subscriber_lagged={count} (application events, not KNX bus telegrams)");
                        if !has_database {
                            eprintln!("No durable history is configured for missed live events");
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => {}
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
    }
    if let Some(sender) = stop_tx.take() {
        let _ = sender.send(());
    }
    task.await??;
    if let Some(error) = terminal_error {
        return Err(Box::new(error));
    }
    Ok(())
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
