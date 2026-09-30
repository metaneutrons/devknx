// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Validated, database-bound snapshots shared by both interactive importers.

use std::path::PathBuf;

use devknx::ets::{CsvEncoding, EtsCatalog, EtsFormat, parse_dpt};
use devknx::storage::CaptureStore;

#[derive(Debug)]
pub struct EtsImportPreview {
    pub database: PathBuf,
    pub file: PathBuf,
    catalog: EtsCatalog,
    identity: same_file::Handle,
}

impl EtsImportPreview {
    pub fn summary(&self) -> String {
        let typed = self
            .catalog
            .groups()
            .filter(|group| !group.dpts.is_empty())
            .count();
        let ambiguous = self
            .catalog
            .groups()
            .filter(|group| group.dpts.len() > 1)
            .count();
        format!(
            "{} group {} · {typed} with DPT · {ambiguous} ambiguous",
            self.catalog.len(),
            if self.catalog.len() == 1 {
                "address"
            } else {
                "addresses"
            }
        )
    }

    pub fn sample_lines(&self) -> Vec<String> {
        self.catalog
            .groups()
            .take(5)
            .map(|group| {
                let mut name: String = group
                    .name
                    .chars()
                    .take(60)
                    .map(|ch| if ch.is_control() { ' ' } else { ch })
                    .collect();
                if group.name.chars().nth(60).is_some() {
                    name.push('…');
                }
                let dpt = match group.dpts.as_slice() {
                    [] => "no DPT".to_owned(),
                    [dpt] => parse_dpt(dpt).map_or_else(|_| dpt.clone(), |dpt| dpt.to_string()),
                    dpts => format!("ambiguous ({})", dpts.len()),
                };
                format!("{} · {name} · {dpt}", group.notation)
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct EtsImportOutcome {
    pub database: PathBuf,
    pub revision: i64,
    pub groups: usize,
}

impl EtsImportOutcome {
    pub fn notice(&self) -> String {
        format!(
            "Imported {} ETS group {} (revision {}); capture history preserved",
            self.groups,
            if self.groups == 1 {
                "address"
            } else {
                "addresses"
            },
            self.revision
        )
    }
}

pub fn prepare(
    database: PathBuf,
    file: PathBuf,
    format: EtsFormat,
    encoding: CsvEncoding,
) -> Result<EtsImportPreview, String> {
    // A missing capture must never become a new, unrelated database on import.
    let _ = CaptureStore::open_existing(&database).map_err(|error| error.to_string())?;
    let identity = same_file::Handle::from_path(&database).map_err(|error| error.to_string())?;
    let catalog =
        EtsCatalog::from_file(&file, format, encoding).map_err(|error| error.to_string())?;
    Ok(EtsImportPreview {
        database,
        file,
        catalog,
        identity,
    })
}

pub fn commit(preview: EtsImportPreview) -> Result<EtsImportOutcome, String> {
    verify_target(&preview)?;
    let mut store = CaptureStore::open_existing_for_ets_import(&preview.database).map_err(|error| {
        format!("ETS import failed: {error}. Disconnect the session using this capture before importing; the daemon and REST may stay running.")
    })?;
    verify_target(&preview)?;
    let revision = store
        .import_ets(&preview.catalog)
        .map_err(|error| error.to_string())?;
    Ok(EtsImportOutcome {
        database: preview.database,
        revision,
        groups: preview.catalog.len(),
    })
}

fn verify_target(preview: &EtsImportPreview) -> Result<(), String> {
    let identity =
        same_file::Handle::from_path(&preview.database).map_err(|error| error.to_string())?;
    if identity != preview.identity {
        return Err("Destination capture was replaced; create a new import preview".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "Main;Middle;Sub;Address\nHome;Lights;Desk;1/2/4\n";

    #[test]
    fn confirmation_commits_the_preview_snapshot_not_a_changed_file() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("capture.sqlite");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        let file = dir.path().join("groups.csv");
        std::fs::write(&file, CSV).unwrap();
        let preview = prepare(
            database.clone(),
            file.clone(),
            EtsFormat::Csv31,
            CsvEncoding::Utf8,
        )
        .unwrap();
        assert!(preview.summary().contains("1 group address"));
        assert!(preview.sample_lines()[0].contains("Desk"));
        std::fs::write(&file, "invalid").unwrap();
        let outcome = commit(preview).unwrap();
        assert_eq!(outcome.revision, 1);
        let store = CaptureStore::open_existing(&database).unwrap();
        assert_eq!(
            store
                .ets_group(devknx::ets::parse_group_address("1/2/4").unwrap())
                .unwrap()
                .unwrap()
                .name,
            "Desk"
        );
    }

    #[test]
    fn preview_cancel_invalid_file_and_busy_writer_do_not_change_revision() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("capture.sqlite");
        let writer = CaptureStore::open_for_ets_import(&database).unwrap();
        let file = dir.path().join("groups.csv");
        std::fs::write(&file, CSV).unwrap();
        drop(
            prepare(
                database.clone(),
                file.clone(),
                EtsFormat::Csv31,
                CsvEncoding::Utf8,
            )
            .unwrap(),
        );
        assert_eq!(writer.ets_revision().unwrap(), None);
        let preview = prepare(
            database.clone(),
            file.clone(),
            EtsFormat::Csv31,
            CsvEncoding::Utf8,
        )
        .unwrap();
        assert!(commit(preview).is_err());
        assert_eq!(writer.ets_revision().unwrap(), None);
        std::fs::write(&file, "invalid").unwrap();
        assert!(prepare(database, file, EtsFormat::Csv31, CsvEncoding::Utf8).is_err());
        assert_eq!(writer.ets_revision().unwrap(), None);
    }

    #[test]
    fn deleted_target_is_not_recreated() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("capture.sqlite");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        let file = dir.path().join("groups.csv");
        std::fs::write(&file, CSV).unwrap();
        let preview = prepare(database.clone(), file, EtsFormat::Csv31, CsvEncoding::Utf8).unwrap();
        std::fs::remove_file(&database).unwrap();
        assert!(commit(preview).is_err());
        assert!(!database.exists());
    }

    #[test]
    fn replaced_database_cannot_receive_a_previous_preview() {
        let dir = tempfile::tempdir().unwrap();
        let database = dir.path().join("capture.sqlite");
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        let file = dir.path().join("groups.csv");
        std::fs::write(&file, CSV).unwrap();
        let preview = prepare(database.clone(), file, EtsFormat::Csv31, CsvEncoding::Utf8).unwrap();
        std::fs::rename(&database, dir.path().join("original.sqlite")).unwrap();
        drop(CaptureStore::open_for_ets_import(&database).unwrap());
        assert!(commit(preview).unwrap_err().contains("replaced"));
        assert_eq!(
            CaptureStore::open_existing(&database)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );
    }
}
