// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use clap::{Parser, Subcommand};
use devknx::capture::CaptureEvent;
use devknx::service::{CaptureService, ReconnectPolicy};
use devknx::storage::CaptureStore;
use knx_rs_ip::{ConnectionSpec, discovery, parse_url};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::Ipv4Addr;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::PathBuf;
use std::time::UNIX_EPOCH;
use tokio::sync::{broadcast, oneshot};

#[cfg(feature = "gui")]
mod gui;

#[derive(Parser)]
#[command(name = "devknx", version, about = "Discover and monitor KNXnet/IP")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
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
    /// Open the native desktop application.
    #[cfg(feature = "gui")]
    Gui,
}

#[tokio::main]
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
        Some(Command::History {
            database,
            after,
            limit,
        }) => {
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            for capture in store.read_after(after, limit)? {
                writeln!(output, "id={} {}", capture.id, format_event(&capture.event))?;
            }
        }
        Some(Command::Export { database, after }) => {
            let store = CaptureStore::open_existing(&database)?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            store.export_csv(&mut output, after)?;
        }
        #[cfg(feature = "gui")]
        Some(Command::Gui) | None => gui::run()?,
        #[cfg(not(feature = "gui"))]
        None => {
            eprintln!("No command specified. Use --help for available commands.");
            std::process::exit(2);
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
