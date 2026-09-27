// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Full-build CLI/TUI/GUI capability ledger with source-anchored differences.
//! Feature-disabled builds omit a surface; this ledger describes `--all-features`.

/// A user-facing operation in at least one application surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    GatewayDiscovery,
    LiveCapture,
    CaptureService,
    RestApi,
    McpServer,
    HistoryRead,
    CsvExport,
    DatabaseBackup,
    ServiceStatus,
    ServiceFollow,
    RouterLossHistory,
    EtsImport,
    EtsLookup,
    WritePreview,
    TypedWrite,
    RawWrite,
    GroupRead,
    OperationAudit,
}

/// All tracked application operations.
pub const ALL: &[Capability] = &[
    Capability::GatewayDiscovery,
    Capability::LiveCapture,
    Capability::CaptureService,
    Capability::RestApi,
    Capability::McpServer,
    Capability::HistoryRead,
    Capability::CsvExport,
    Capability::DatabaseBackup,
    Capability::ServiceStatus,
    Capability::ServiceFollow,
    Capability::RouterLossHistory,
    Capability::EtsImport,
    Capability::EtsLookup,
    Capability::WritePreview,
    Capability::TypedWrite,
    Capability::RawWrite,
    Capability::GroupRead,
    Capability::OperationAudit,
];

/// Human-facing adapter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Cli,
    Tui,
    Gui,
}

/// A tracked operation and a findable implementation/control fragment.
#[derive(Clone, Copy, Debug)]
pub struct Declaration {
    pub capability: Capability,
    pub source_anchor: &'static str,
}

/// Command dispatch anchors in `src/main.rs`.
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
        capability: Capability::RestApi,
        source_anchor: "Some(Command::Api {",
    },
    Declaration {
        capability: Capability::McpServer,
        source_anchor: "Some(Command::Mcp {",
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
    Declaration {
        capability: Capability::RouterLossHistory,
        source_anchor: "Some(Command::RouterLosses {",
    },
    Declaration {
        capability: Capability::EtsImport,
        source_anchor: "Some(Command::EtsImport {",
    },
    Declaration {
        capability: Capability::EtsLookup,
        source_anchor: "Some(Command::EtsLookup {",
    },
    Declaration {
        capability: Capability::WritePreview,
        source_anchor: "Some(Command::WritePreview {",
    },
    Declaration {
        capability: Capability::TypedWrite,
        source_anchor: "Some(Command::Write {",
    },
    Declaration {
        capability: Capability::RawWrite,
        source_anchor: "Some(Command::WriteRaw {",
    },
    Declaration {
        capability: Capability::GroupRead,
        source_anchor: "Some(Command::Read {",
    },
    Declaration {
        capability: Capability::OperationAudit,
        source_anchor: "Some(Command::Audit {",
    },
];

/// Terminal control anchors in `src/tui.rs`.
pub const TUI: &[Declaration] = &[
    Declaration {
        capability: Capability::GatewayDiscovery,
        source_anchor: "KeyCode::Char('d') =>",
    },
    Declaration {
        capability: Capability::LiveCapture,
        source_anchor: "follower.receiver.try_iter()",
    },
    Declaration {
        capability: Capability::CaptureService,
        source_anchor: "interface::connect_owner",
    },
    Declaration {
        capability: Capability::HistoryRead,
        source_anchor: "model.reload_history()",
    },
    Declaration {
        capability: Capability::CsvExport,
        source_anchor: "interface::export_csv",
    },
    Declaration {
        capability: Capability::ServiceStatus,
        source_anchor: "self.model.state",
    },
    Declaration {
        capability: Capability::ServiceFollow,
        source_anchor: "Follower::start(database)",
    },
    Declaration {
        capability: Capability::EtsLookup,
        source_anchor: "row.label.as_deref()",
    },
    Declaration {
        capability: Capability::WritePreview,
        source_anchor: "self.model.preview_write",
    },
    Declaration {
        capability: Capability::TypedWrite,
        source_anchor: "interface::write_request",
    },
    Declaration {
        capability: Capability::GroupRead,
        source_anchor: "interface::read_request",
    },
];

/// Desktop control anchors in `src/gui.rs`.
pub const GUI: &[Declaration] = &[
    Declaration {
        capability: Capability::GatewayDiscovery,
        source_anchor: "ui.button(\"Discover gateways\")",
    },
    Declaration {
        capability: Capability::LiveCapture,
        source_anchor: "follower.receiver.try_iter()",
    },
    Declaration {
        capability: Capability::CaptureService,
        source_anchor: "interface::connect_owner",
    },
    Declaration {
        capability: Capability::HistoryRead,
        source_anchor: "model.reload_history()",
    },
    Declaration {
        capability: Capability::CsvExport,
        source_anchor: "interface::export_csv",
    },
    Declaration {
        capability: Capability::ServiceStatus,
        source_anchor: "model.state",
    },
    Declaration {
        capability: Capability::ServiceFollow,
        source_anchor: "Follower::start(database.clone())",
    },
    Declaration {
        capability: Capability::EtsLookup,
        source_anchor: "model.ets_for_address",
    },
    Declaration {
        capability: Capability::WritePreview,
        source_anchor: "model.preview_write",
    },
    Declaration {
        capability: Capability::TypedWrite,
        source_anchor: "interface::write_request",
    },
    Declaration {
        capability: Capability::GroupRead,
        source_anchor: "interface::read_request",
    },
];

/// Deliberate difference from the CLI, which remains the complete expert surface.
#[derive(Clone, Copy, Debug)]
pub struct Gap {
    pub capability: Capability,
    pub missing_on: Surface,
    pub reason: &'static str,
    pub target_issue: &'static str,
}

const M4: &str = "https://github.com/metaneutrons/devknx/issues/7";
const M5: &str = "https://github.com/metaneutrons/devknx/issues/8";

/// Explicit exceptions. Import and backup require exclusive database ownership;
/// raw sending and audit inspection remain expert CLI controls in this slice.
pub const GAPS: &[Gap] = &[
    Gap {
        capability: Capability::RestApi,
        missing_on: Surface::Tui,
        reason: "REST listener configuration is an explicit CLI service operation",
        target_issue: M5,
    },
    Gap {
        capability: Capability::McpServer,
        missing_on: Surface::Tui,
        reason: "MCP stdio is an explicit CLI process for a local client",
        target_issue: M5,
    },
    Gap {
        capability: Capability::McpServer,
        missing_on: Surface::Gui,
        reason: "MCP stdio is an explicit CLI process for a local client",
        target_issue: M5,
    },
    Gap {
        capability: Capability::RestApi,
        missing_on: Surface::Gui,
        reason: "REST listener configuration is an explicit CLI service operation",
        target_issue: M5,
    },
    Gap {
        capability: Capability::DatabaseBackup,
        missing_on: Surface::Tui,
        reason: "Snapshot is an expert CLI maintenance operation",
        target_issue: M4,
    },
    Gap {
        capability: Capability::DatabaseBackup,
        missing_on: Surface::Gui,
        reason: "Snapshot is an expert CLI maintenance operation",
        target_issue: M4,
    },
    Gap {
        capability: Capability::RouterLossHistory,
        missing_on: Surface::Tui,
        reason: "Live router diagnostics appear as notices; durable history is CLI-only",
        target_issue: M4,
    },
    Gap {
        capability: Capability::RouterLossHistory,
        missing_on: Surface::Gui,
        reason: "Live router diagnostics appear as notices; durable history is CLI-only",
        target_issue: M4,
    },
    Gap {
        capability: Capability::EtsImport,
        missing_on: Surface::Tui,
        reason: "Import requires the service to stop and an exclusive writer lease",
        target_issue: M4,
    },
    Gap {
        capability: Capability::EtsImport,
        missing_on: Surface::Gui,
        reason: "Import requires the service to stop and an exclusive writer lease",
        target_issue: M4,
    },
    Gap {
        capability: Capability::RawWrite,
        missing_on: Surface::Tui,
        reason: "Expert raw sending is deliberately CLI-only",
        target_issue: M4,
    },
    Gap {
        capability: Capability::RawWrite,
        missing_on: Surface::Gui,
        reason: "Expert raw sending is deliberately CLI-only",
        target_issue: M4,
    },
    Gap {
        capability: Capability::OperationAudit,
        missing_on: Surface::Tui,
        reason: "Durable audit inspection is currently CLI-only",
        target_issue: M4,
    },
    Gap {
        capability: Capability::OperationAudit,
        missing_on: Surface::Gui,
        reason: "Durable audit inspection is currently CLI-only",
        target_issue: M4,
    },
];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    const CLI_SOURCE: &str = include_str!("main.rs");
    const TUI_SOURCE: &str = include_str!("tui.rs");
    const GUI_SOURCE: &str = include_str!("gui.rs");

    #[expect(clippy::too_many_lines, reason = "one source-anchored registry audit")]
    fn audit(sources: [&str; 3], gaps: &[Gap]) -> Result<(), String> {
        let known: HashSet<_> = ALL.iter().copied().collect();
        if known.len() != ALL.len() {
            return Err("duplicate capability ID".into());
        }
        let mut sets = Vec::new();
        for (surface, declarations, source) in [
            (Surface::Cli, CLI, sources[0]),
            (Surface::Tui, TUI, sources[1]),
            (Surface::Gui, GUI, sources[2]),
        ] {
            let mut seen = HashSet::new();
            for declaration in declarations {
                if !known.contains(&declaration.capability) || !seen.insert(declaration.capability)
                {
                    return Err(format!("unknown or duplicate capability on {surface:?}"));
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
            sets.push(seen);
        }
        if sets[0] != known {
            return Err("CLI capability list is incomplete".into());
        }
        let implemented_cli: HashSet<_> = sources[0]
            .lines()
            .filter_map(|line| line.trim_start().strip_prefix("Some(Command::"))
            .filter_map(|rest| rest.split(|ch: char| !ch.is_ascii_alphanumeric()).next())
            .filter(|name| !matches!(*name, "Gui" | "Tui"))
            .collect();
        let declared_cli: HashSet<_> = CLI
            .iter()
            .filter_map(|declaration| declaration.source_anchor.strip_prefix("Some(Command::"))
            .filter_map(|rest| rest.split(|ch: char| !ch.is_ascii_alphanumeric()).next())
            .collect();
        if implemented_cli != declared_cli {
            return Err("CLI commands differ from declarations".into());
        }

        let mut gap_ids = HashSet::new();
        for gap in gaps {
            if !matches!(gap.missing_on, Surface::Tui | Surface::Gui)
                || !gap_ids.insert((gap.capability, gap.missing_on))
                || gap.reason.trim().is_empty()
                || !gap.target_issue.starts_with("https://github.com/")
            {
                return Err(format!("invalid gap for {:?}", gap.capability));
            }
            let index = if gap.missing_on == Surface::Tui { 1 } else { 2 };
            if sets[index].contains(&gap.capability) {
                return Err(format!("stale gap for {:?}", gap.capability));
            }
        }
        for capability in ALL {
            for (index, surface) in [(1, Surface::Tui), (2, Surface::Gui)] {
                if sets[index].contains(capability) == gap_ids.contains(&(*capability, surface)) {
                    return Err(format!(
                        "unrecorded difference for {capability:?} on {surface:?}"
                    ));
                }
            }
        }
        for needle in [
            "Open Capture",
            "Discover gateways",
            "Settings…",
            "Connect",
            "Disconnect",
            "Export…",
            "Read…",
            "Write…",
            "capture-filter",
            "raw cEMI",
        ] {
            if !sources[2].contains(needle) {
                return Err(format!("missing GUI control {needle}"));
            }
        }
        for needle in [
            "'q'",
            "'/'",
            "'r'",
            "'w'",
            "'e'",
            "'h'",
            "'d'",
            "'c'",
            "'s'",
            "KeyCode::Down",
            "KeyCode::Up",
            "raw cEMI",
        ] {
            if !sources[1].contains(needle) {
                return Err(format!("missing TUI control {needle}"));
            }
        }
        Ok(())
    }

    #[test]
    fn full_build_sources_match_registry() {
        audit([CLI_SOURCE, TUI_SOURCE, GUI_SOURCE], GAPS).unwrap();
    }

    #[test]
    fn removed_or_unregistered_controls_fail() {
        assert!(
            audit(
                [
                    &CLI_SOURCE.replace("Some(Command::History {", "Some(Command::Gone {"),
                    TUI_SOURCE,
                    GUI_SOURCE
                ],
                GAPS
            )
            .is_err()
        );
        assert!(
            audit(
                [
                    CLI_SOURCE,
                    &TUI_SOURCE.replace("KeyCode::Char('d') =>", "KeyCode::Char('x') =>"),
                    GUI_SOURCE
                ],
                GAPS
            )
            .is_err()
        );
        assert!(
            audit(
                [
                    CLI_SOURCE,
                    TUI_SOURCE,
                    &GUI_SOURCE.replace("ui.button(\"Discover gateways\")", "ui.button(\"Other\")")
                ],
                GAPS
            )
            .is_err()
        );
        assert!(audit([CLI_SOURCE, TUI_SOURCE, GUI_SOURCE], &GAPS[1..]).is_err());
        let mut stale = GAPS.to_vec();
        stale.push(Gap {
            capability: Capability::GatewayDiscovery,
            missing_on: Surface::Gui,
            reason: "invalid counter-probe",
            target_issue: M4,
        });
        assert!(audit([CLI_SOURCE, TUI_SOURCE, GUI_SOURCE], &stale).is_err());
    }
}
