// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

use std::net::Ipv4Addr;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;
use knx_rs_ip::discovery::{self, GatewayInfo};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let icon =
        image::load_from_memory(include_bytes!("../resources/png/devknx-256.png"))?.to_rgba8();
    let (width, height) = icon.dimensions();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_app_id("devknx")
            .with_title("devknx")
            .with_icon(egui::IconData {
                rgba: icon.into_raw(),
                width,
                height,
            })
            .with_inner_size([720.0, 480.0])
            .with_min_inner_size([480.0, 300.0]),
        ..Default::default()
    };
    eframe::run_native(
        "devknx",
        options,
        Box::new(|_cc| Ok(Box::<DiscoveryApp>::default())),
    )?;
    Ok(())
}

#[derive(Default)]
struct DiscoveryApp {
    receiver: Option<Receiver<Result<Vec<GatewayInfo>, String>>>,
    gateways: Vec<GatewayInfo>,
    error: Option<String>,
}

impl DiscoveryApp {
    fn discover(&mut self) {
        let (sender, receiver) = mpsc::channel();
        self.receiver = Some(receiver);
        self.error = None;
        std::thread::spawn(move || {
            let outcome = tokio::runtime::Runtime::new()
                .map_err(|error| error.to_string())
                .and_then(|runtime| {
                    runtime
                        .block_on(discovery::discover(Ipv4Addr::UNSPECIFIED))
                        .map_err(|error| error.to_string())
                });
            let _ = sender.send(outcome);
        });
    }
}

impl eframe::App for DiscoveryApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(receiver) = &self.receiver {
            match receiver.try_recv() {
                Ok(Ok(gateways)) => {
                    self.gateways = gateways;
                    self.receiver = None;
                }
                Ok(Err(error)) => {
                    self.error = Some(error);
                    self.receiver = None;
                }
                Err(mpsc::TryRecvError::Empty) => ui
                    .ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100)),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Gateway discovery stopped unexpectedly".to_owned());
                    self.receiver = None;
                }
            }
        }

        ui.heading("KNXnet/IP gateways");
        ui.add_space(12.0);
        if ui
            .add_enabled(
                self.receiver.is_none(),
                egui::Button::new("Discover gateways"),
            )
            .clicked()
        {
            self.discover();
        }
        if self.receiver.is_some() {
            ui.spinner();
        }
        if let Some(error) = &self.error {
            ui.colored_label(egui::Color32::RED, error);
        }
        ui.add_space(12.0);
        for gateway in &self.gateways {
            ui.horizontal(|ui| {
                ui.strong(&gateway.name);
                ui.monospace(gateway.address.to_string());
            });
        }
    }
}
