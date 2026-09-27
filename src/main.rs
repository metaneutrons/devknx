// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use clap::{Parser, Subcommand};
use knx_rs_ip::discovery;
use std::net::Ipv4Addr;

#[cfg(feature = "gui")]
mod gui;

#[derive(Parser)]
#[command(name = "devknx", version, about = "Discover KNXnet/IP gateways")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Discover KNXnet/IP gateways on the local network.
    Discover,
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
