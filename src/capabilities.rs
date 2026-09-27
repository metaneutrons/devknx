// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Full-build capability registry and declared CLI/GUI differences.
//!
//! Each surface has a source anchor for every declared capability. The audit
//! fails if an anchor disappears or the difference changes without an entry in
//! [`GAPS`]. It reads source text and cannot evaluate conditional compilation:
//! this ledger describes a full build, even when a headless build omits the GUI.
//! Future TUI, REST, and MCP surfaces must join the same registry when added.

/// User-facing operation implemented by at least one surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// Search the local network for KNXnet/IP gateways.
    GatewayDiscovery,
    /// Receive and display KNXnet/IP telegrams.
    LiveCapture,
    /// Run independent, persistent foreground capture.
    CaptureService,
    /// Read committed telegrams from SQLite by event ID.
    HistoryRead,
    /// Export committed telegrams as CSV.
    CsvExport,
    /// Create a consistent, non-overwriting SQLite snapshot.
    DatabaseBackup,
    /// Query the independent capture process's current state.
    ServiceStatus,
    /// Follow the independent process's live state and captures.
    ServiceFollow,
}

/// All currently implemented user-facing operations.
pub const ALL: &[Capability] = &[
    Capability::GatewayDiscovery,
    Capability::LiveCapture,
    Capability::CaptureService,
    Capability::HistoryRead,
    Capability::CsvExport,
    Capability::DatabaseBackup,
    Capability::ServiceStatus,
    Capability::ServiceFollow,
];

/// An application surface that exposes capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// Command-line interface.
    Cli,
    /// Native desktop GUI.
    Gui,
}

/// A surface declaration tied to a findable implementation anchor.
#[derive(Clone, Copy, Debug)]
pub struct Declaration {
    /// Declared operation.
    pub capability: Capability,
    /// Exact source fragment that must remain present in the surface.
    pub source_anchor: &'static str,
}

/// Operations exposed by the CLI in `src/main.rs`.
pub const CLI: &[Declaration] = &[
    Declaration {
        capability: Capability::GatewayDiscovery,
        source_anchor: "Some(Command::Discover) =>",
    },
    Declaration {
        capability: Capability::LiveCapture,
        source_anchor: "Some(Command::Monitor {",
    },
    Declaration {
        capability: Capability::CaptureService,
        source_anchor: "Some(Command::Serve {",
    },
    Declaration {
        capability: Capability::HistoryRead,
        source_anchor: "Some(Command::History {",
    },
    Declaration {
        capability: Capability::CsvExport,
        source_anchor: "Some(Command::Export {",
    },
    Declaration {
        capability: Capability::DatabaseBackup,
        source_anchor: "Some(Command::Backup {",
    },
    Declaration {
        capability: Capability::ServiceStatus,
        source_anchor: "Some(Command::Status {",
    },
    Declaration {
        capability: Capability::ServiceFollow,
        source_anchor: "Some(Command::Follow {",
    },
];

/// Operations exposed by the native GUI in `src/gui.rs`.
pub const GUI: &[Declaration] = &[Declaration {
    capability: Capability::GatewayDiscovery,
    source_anchor: "egui::Button::new(\"Discover gateways\")",
}];

/// One intentional asymmetry between two application surfaces.
#[derive(Clone, Copy, Debug)]
pub struct Gap {
    /// Operation that is not yet offered on both surfaces.
    pub capability: Capability,
    /// Surface currently offering the operation.
    pub present_on: Surface,
    /// Surface that still needs the operation.
    pub missing_on: Surface,
    /// Why the asymmetry exists today.
    pub reason: &'static str,
    /// Tracking issue for eliminating or revisiting the difference.
    pub target_issue: &'static str,
}

/// Reviewed CLI/GUI differences. A new or removed difference requires an edit here.
pub const GAPS: &[Gap] = &[
    Gap {
        capability: Capability::LiveCapture,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "The GUI is a discovery shell; live capture is an M4 interface deliverable.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::CaptureService,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "The GUI cannot yet start or attach to the independent capture process.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::HistoryRead,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "The GUI has no persistent-service history view yet.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::CsvExport,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "CSV export exists only in the CLI until the GUI shares the history operation.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::DatabaseBackup,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "Database snapshots are available in the CLI; the GUI has no history controls yet.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::ServiceStatus,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "The GUI has not yet attached to the independent capture process.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
    Gap {
        capability: Capability::ServiceFollow,
        present_on: Surface::Cli,
        missing_on: Surface::Gui,
        reason: "The GUI has not yet attached to the independent capture process.",
        target_issue: "https://github.com/metaneutrons/devknx/issues/7",
    },
];

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    const CLI_SOURCE: &str = include_str!("main.rs");
    const GUI_SOURCE: &str = include_str!("gui.rs");

    fn command_name(fragment: &str) -> &str {
        fragment
            .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
            .next()
            .unwrap_or("")
    }

    fn audit(cli_source: &str, gui_source: &str, gaps: &[Gap]) -> Result<(), String> {
        let known: HashSet<_> = ALL.iter().copied().collect();
        if known.len() != ALL.len() {
            return Err("duplicate capability ID".to_owned());
        }
        for (surface, declarations, source) in [
            (Surface::Cli, CLI, cli_source),
            (Surface::Gui, GUI, gui_source),
        ] {
            let mut seen = HashSet::new();
            for declaration in declarations {
                if !known.contains(&declaration.capability) {
                    return Err(format!("unknown capability on {surface:?}"));
                }
                if !seen.insert(declaration.capability) {
                    return Err(format!("duplicate declaration on {surface:?}"));
                }
                if declaration.source_anchor.is_empty()
                    || !source.contains(declaration.source_anchor)
                {
                    return Err(format!(
                        "missing {surface:?} source anchor for {:?}",
                        declaration.capability
                    ));
                }
            }
        }

        let implemented_cli: HashSet<_> = cli_source
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("Some(Command::"))
            .map(command_name)
            .filter(|name| *name != "Gui")
            .collect();
        let declared_cli: HashSet<_> = CLI
            .iter()
            .filter_map(|entry| entry.source_anchor.strip_prefix("Some(Command::"))
            .map(command_name)
            .collect();
        if implemented_cli != declared_cli {
            return Err("CLI commands differ from declared capabilities".to_owned());
        }
        let gui_buttons = gui_source.matches("egui::Button::new(").count();
        let declared_buttons = GUI
            .iter()
            .filter(|entry| entry.source_anchor.starts_with("egui::Button::new("))
            .count();
        if gui_buttons != declared_buttons {
            return Err("GUI buttons differ from declared capabilities".to_owned());
        }

        let cli: HashSet<_> = CLI.iter().map(|entry| entry.capability).collect();
        let gui: HashSet<_> = GUI.iter().map(|entry| entry.capability).collect();
        let mut gap_ids = HashSet::new();
        for gap in gaps {
            if !gap_ids.insert(gap.capability) {
                return Err(format!("duplicate ledger gap for {:?}", gap.capability));
            }
            if gap.reason.trim().is_empty() || !gap.target_issue.starts_with("https://github.com/")
            {
                return Err(format!("unexplained ledger gap for {:?}", gap.capability));
            }
            let expected = match (cli.contains(&gap.capability), gui.contains(&gap.capability)) {
                (true, false) => (Surface::Cli, Surface::Gui),
                (false, true) => (Surface::Gui, Surface::Cli),
                _ => return Err(format!("stale ledger gap for {:?}", gap.capability)),
            };
            if (gap.present_on, gap.missing_on) != expected {
                return Err(format!("reversed ledger gap for {:?}", gap.capability));
            }
        }

        for capability in ALL {
            let on_cli = cli.contains(capability);
            let on_gui = gui.contains(capability);
            if !on_cli && !on_gui {
                return Err(format!("orphan capability: {capability:?}"));
            }
            if (on_cli != on_gui) != gap_ids.contains(capability) {
                return Err(format!("unrecorded difference for {capability:?}"));
            }
        }
        Ok(())
    }

    #[test]
    fn source_anchors_and_gap_ledger_match_full_build() {
        audit(CLI_SOURCE, GUI_SOURCE, GAPS).expect("capability ledger matches source");
    }

    #[test]
    fn removed_control_breaks_the_audit() {
        let without_history =
            CLI_SOURCE.replace("Some(Command::History {", "Some(Command::Removed {");
        assert!(audit(&without_history, GUI_SOURCE, GAPS).is_err());

        let added_command = CLI_SOURCE.replace(
            "Some(Command::Discover) =>",
            "Some(Command::Unknown) => {}\n        Some(Command::Discover) =>",
        );
        assert!(audit(&added_command, GUI_SOURCE, GAPS).is_err());

        let added_button = GUI_SOURCE.replace(
            "egui::Button::new(\"Discover gateways\")",
            "egui::Button::new(\"New control\"); egui::Button::new(\"Discover gateways\")",
        );
        assert!(audit(CLI_SOURCE, &added_button, GAPS).is_err());
    }

    #[test]
    fn unrecorded_or_stale_difference_breaks_the_audit() {
        let missing_gap = GAPS[1..].to_vec();
        assert!(audit(CLI_SOURCE, GUI_SOURCE, &missing_gap).is_err());

        let mut stale = GAPS.to_vec();
        stale.push(Gap {
            capability: Capability::GatewayDiscovery,
            present_on: Surface::Cli,
            missing_on: Surface::Gui,
            reason: "deliberately invalid counter-probe",
            target_issue: "https://github.com/metaneutrons/devknx/issues/7",
        });
        assert!(audit(CLI_SOURCE, GUI_SOURCE, &stale).is_err());
    }
}
