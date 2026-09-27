// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use clap::{Parser, Subcommand};
use devknx::capture::{CaptureEndpoint, CaptureEvent, CaptureExit, capture_until};
use knx_rs_ip::{connect, discovery, parse_url};
use std::fmt::Write as _;
use std::io::{self, Write as _};
use std::net::Ipv4Addr;
use std::time::UNIX_EPOCH;

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
        Some(Command::Monitor { endpoint }) => {
            let spec = parse_url(&endpoint)?;
            let capture_endpoint = CaptureEndpoint::from(spec.clone());
            let mut connection = connect(spec).await?;
            let stdout = io::stdout();
            let mut output = stdout.lock();
            let exit = capture_until(
                &mut connection,
                capture_endpoint,
                |event| {
                    writeln!(output, "{}", format_event(&event))?;
                    output.flush()
                },
                tokio::signal::ctrl_c(),
            )
            .await?;
            if exit == CaptureExit::Disconnected {
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionAborted,
                    format!("KNXnet/IP connection closed: {capture_endpoint}"),
                )
                .into());
            }
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
        "timestamp_ms={timestamp_ms} endpoint={} direction=received source={} destination={} service={:?} message_code=0x{:02x} cemi={raw_hex}",
        event.endpoint(),
        frame.source_address(),
        frame.destination_address(),
        event.group_service(),
        frame.message_code_raw(),
    )
}

#[cfg(test)]
mod tests {
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
