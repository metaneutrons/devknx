// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Versioned SQLite history for raw KNX telegrams.

use std::io::{self, Write};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use knx_rs_core::cemi::CemiFrame;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use thiserror::Error;

use crate::capture::{CaptureDirection, CaptureEndpoint, CaptureEvent};

const SCHEMA_VERSION: i64 = 1;
/// Maximum number of rows returned by one cursor query.
pub const MAX_PAGE_SIZE: u32 = 1_000;
const EXPORT_PAGE_SIZE: NonZeroU32 = NonZeroU32::new(500).expect("nonzero export page size");

const SCHEMA_V1: &str = "
    CREATE TABLE capture_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
        transport TEXT NOT NULL CHECK (transport IN ('tunnel', 'router')),
        endpoint TEXT NOT NULL,
        direction TEXT NOT NULL CHECK (direction IN ('received', 'sent')),
        message_code INTEGER NOT NULL,
        source_raw INTEGER NOT NULL,
        destination_raw INTEGER NOT NULL,
        destination_is_group INTEGER NOT NULL CHECK (destination_is_group IN (0, 1)),
        service_code INTEGER CHECK (service_code BETWEEN 0 AND 1023),
        payload BLOB NOT NULL,
        raw_cemi BLOB NOT NULL
    );
    CREATE INDEX idx_capture_time ON capture_events(observed_at_ms);
    CREATE INDEX idx_capture_destination ON capture_events(destination_raw, id);
    PRAGMA user_version = 1;
";

/// Errors opening, writing, reading, or exporting capture history.
#[derive(Debug, Error)]
pub enum StorageError {
    /// SQLite returned an error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Filesystem or export writer returned an error.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    /// The database schema is newer or otherwise unsupported.
    #[error("unsupported capture schema version {0}")]
    UnsupportedSchema(i64),
    /// A capture table exists without a version; migration must not guess its layout.
    #[error("capture table exists without a schema version")]
    UnversionedCaptureTable,
    /// The file is another application database and must not be modified.
    #[error("database contains an unrelated table: {0}")]
    UnrecognizedDatabase(String),
    /// A read-only store cannot accept frames.
    #[error("capture store is read-only")]
    ReadOnly,
    /// Cursor IDs are nonnegative, with zero meaning the beginning.
    #[error("capture cursor must be nonnegative: {0}")]
    InvalidCursor(i64),
    /// Pages are bounded so callers cannot allocate an unlimited result.
    #[error("capture page size exceeds {MAX_PAGE_SIZE}: {0}")]
    PageTooLarge(u32),
    /// The event timestamp cannot be represented as a SQLite millisecond timestamp.
    #[error("capture timestamp is outside the supported Unix millisecond range")]
    InvalidTimestamp,
    /// Stored derived fields disagree with the original cEMI frame or are invalid.
    #[error("corrupt capture row {id}: {reason}")]
    CorruptRow {
        /// Capture ID.
        id: i64,
        /// Validation failure.
        reason: String,
    },
}

/// One durable capture with its monotonic database ID.
#[derive(Debug)]
pub struct StoredCapture {
    /// Monotonic ID, never reused after retention pruning.
    pub id: i64,
    /// The raw-preserving capture event.
    pub event: CaptureEvent,
}

/// Single-owner SQLite capture store. A later daemon will own this writer.
pub struct CaptureStore {
    connection: Connection,
    max_events: Option<NonZeroU32>,
}

impl CaptureStore {
    /// Create or migrate a writable database and apply the retention cap.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failure, an unsupported schema, or migration failure.
    pub fn open(path: &Path, max_events: NonZeroU32) -> Result<Self, StorageError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let database_path = normalized_sqlite_path(path)?;
        #[cfg(unix)]
        prepare_private_file(&database_path)?;
        let mut connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        migrate(&mut connection)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        prune(&transaction, max_events)?;
        transaction.commit()?;
        Ok(Self {
            connection,
            max_events: Some(max_events),
        })
    }

    /// Open an existing database for history reads without creating or migrating it.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, unreadable, or not schema version 1.
    pub fn open_existing(path: &Path) -> Result<Self, StorageError> {
        let database_path = normalized_sqlite_path(path)?;
        let connection = Connection::open_with_flags(
            &database_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        require_schema(&connection)?;
        Ok(Self {
            connection,
            max_events: None,
        })
    }

    /// Atomically append a raw event and prune the oldest rows to the cap.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is read-only or the transaction fails.
    pub fn insert(&mut self, event: &CaptureEvent) -> Result<i64, StorageError> {
        let max_events = self.max_events.ok_or(StorageError::ReadOnly)?;
        let timestamp_ms = i64::try_from(
            event
                .observed_at()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| StorageError::InvalidTimestamp)?
                .as_millis(),
        )
        .map_err(|_| StorageError::InvalidTimestamp)?;
        let (transport, address) = endpoint_fields(event.endpoint());
        let direction = event.direction().as_str();
        let frame = event.frame();
        let is_group = i64::from(matches!(
            frame.destination_address(),
            knx_rs_core::address::DestinationAddress::Group(_)
        ));
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO capture_events (
                observed_at_ms, transport, endpoint, direction, message_code,
                source_raw, destination_raw, destination_is_group, service_code,
                payload, raw_cemi
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                timestamp_ms,
                transport,
                address.to_string(),
                direction,
                i64::from(frame.message_code_raw()),
                i64::from(frame.source_address().raw()),
                i64::from(frame.destination_address_raw()),
                is_group,
                service_code(frame),
                frame.payload(),
                frame.as_bytes(),
            ],
        )?;
        let id = transaction.last_insert_rowid();
        prune(&transaction, max_events)?;
        transaction.commit()?;
        Ok(id)
    }

    /// Read events strictly after `cursor`, ordered by ID.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, SQLite failure, or corrupt stored data.
    pub fn read_after(
        &self,
        cursor: i64,
        limit: NonZeroU32,
    ) -> Result<Vec<StoredCapture>, StorageError> {
        self.read_after_up_to(cursor, limit, i64::MAX)
    }

    fn read_after_up_to(
        &self,
        cursor: i64,
        limit: NonZeroU32,
        upper_id: i64,
    ) -> Result<Vec<StoredCapture>, StorageError> {
        if cursor < 0 {
            return Err(StorageError::InvalidCursor(cursor));
        }
        if limit.get() > MAX_PAGE_SIZE {
            return Err(StorageError::PageTooLarge(limit.get()));
        }
        let mut statement = self.connection.prepare(
            "SELECT id, observed_at_ms, transport, endpoint, direction, message_code,
                    source_raw, destination_raw, destination_is_group, service_code,
                    payload, raw_cemi
             FROM capture_events WHERE id > ?1 AND id <= ?2 ORDER BY id LIMIT ?3",
        )?;
        let mut rows = statement.query(params![cursor, upper_id, limit.get()])?;
        let mut captures = Vec::new();
        while let Some(row) = rows.next()? {
            captures.push(decode_row(row)?);
        }
        Ok(captures)
    }

    /// Stream all rows after a cursor to a CSV writer without loading the full history.
    ///
    /// # Errors
    ///
    /// Returns a query, corruption, or writer error. A partial export must be discarded.
    pub fn export_csv<W: Write>(&self, writer: &mut W, after: i64) -> Result<u64, StorageError> {
        if after < 0 {
            return Err(StorageError::InvalidCursor(after));
        }
        let upper_id: i64 = self.connection.query_row(
            "SELECT COALESCE(MAX(id), 0) FROM capture_events",
            [],
            |row| row.get(0),
        )?;
        writer.write_all(b"id,observed_at_ms,transport,endpoint,direction,source,destination,service,message_code,raw_cemi\n")?;
        let mut cursor = after;
        let mut count = 0_u64;
        loop {
            let page = self.read_after_up_to(cursor, EXPORT_PAGE_SIZE, upper_id)?;
            if page.is_empty() {
                break;
            }
            for capture in page {
                let frame = capture.event.frame();
                let (transport, address) = endpoint_fields(capture.event.endpoint());
                let timestamp_ms = capture
                    .event
                    .observed_at()
                    .duration_since(UNIX_EPOCH)
                    .map_err(|_| StorageError::InvalidTimestamp)?
                    .as_millis();
                writeln!(
                    writer,
                    "{},{timestamp_ms},{transport},{address},{},{},{},{},0x{:02x},{}",
                    capture.id,
                    capture.event.direction(),
                    frame.source_address(),
                    frame.destination_address(),
                    service_label(frame),
                    frame.message_code_raw(),
                    hex(frame.as_bytes()),
                )?;
                cursor = capture.id;
                count += 1;
            }
        }
        Ok(count)
    }
}

fn normalized_sqlite_path(path: &Path) -> Result<PathBuf, StorageError> {
    if path == Path::new(":memory:") {
        return Ok(path.to_path_buf());
    }
    let file_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing database filename"))?;
    #[cfg(unix)]
    {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        Ok(parent.canonicalize()?.join(file_name))
    }
    #[cfg(not(unix))]
    {
        let _ = file_name;
        Ok(path.to_path_buf())
    }
}

#[cfg(unix)]
fn prepare_private_file(path: &Path) -> Result<(), StorageError> {
    use std::fs::OpenOptions;
    use std::os::unix::fs::OpenOptionsExt as _;

    if path != Path::new(":memory:") {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => drop(file),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn migrate(connection: &mut Connection) -> Result<(), StorageError> {
    let version = schema_version(connection)?;
    match version {
        0 => {
            let existing: i64 = connection.query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'capture_events'",
                [],
                |row| row.get(0),
            )?;
            if existing != 0 {
                return Err(StorageError::UnversionedCaptureTable);
            }
            let unrelated: Option<String> = connection
                .query_row(
                    "SELECT name FROM sqlite_master
                     WHERE type = 'table' AND name NOT GLOB 'sqlite_*'
                     ORDER BY name LIMIT 1",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(table) = unrelated {
                return Err(StorageError::UnrecognizedDatabase(table));
            }
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCHEMA_V1)?;
            transaction.commit()?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        other => Err(StorageError::UnsupportedSchema(other)),
    }
}

fn require_schema(connection: &Connection) -> Result<(), StorageError> {
    let version = schema_version(connection)?;
    if version == SCHEMA_VERSION {
        Ok(())
    } else {
        Err(StorageError::UnsupportedSchema(version))
    }
}

fn schema_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn prune(transaction: &Transaction<'_>, max_events: NonZeroU32) -> Result<(), StorageError> {
    transaction.execute(
        "DELETE FROM capture_events WHERE id <= (
            SELECT id FROM capture_events ORDER BY id DESC LIMIT 1 OFFSET ?1
        )",
        [max_events.get()],
    )?;
    Ok(())
}

const fn endpoint_fields(endpoint: CaptureEndpoint) -> (&'static str, SocketAddr) {
    match endpoint {
        CaptureEndpoint::Tunnel(address) => ("tunnel", address),
        CaptureEndpoint::Router(address) => ("router", address),
    }
}

fn service_code(frame: &CemiFrame) -> Option<i64> {
    frame
        .tpdu()
        .and_then(|tpdu| tpdu.apdu().map(|apdu| i64::from(apdu.apdu_type as u16)))
}

fn service_label(frame: &CemiFrame) -> String {
    frame
        .tpdu()
        .and_then(|tpdu| tpdu.apdu().map(|apdu| format!("{:?}", apdu.apdu_type)))
        .unwrap_or_else(|| "Undecodable".to_owned())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut result = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(result, "{byte:02x}").expect("writing to String cannot fail");
    }
    result
}

fn decode_row(row: &rusqlite::Row<'_>) -> Result<StoredCapture, StorageError> {
    let id: i64 = row.get(0)?;
    let corrupt = |reason: &str| StorageError::CorruptRow {
        id,
        reason: reason.to_owned(),
    };
    let timestamp_ms: i64 = row.get(1)?;
    let timestamp = u64::try_from(timestamp_ms)
        .ok()
        .and_then(|ms| UNIX_EPOCH.checked_add(Duration::from_millis(ms)))
        .ok_or_else(|| corrupt("invalid timestamp"))?;
    let transport: String = row.get(2)?;
    let address: String = row.get(3)?;
    let address: SocketAddr = address.parse().map_err(|_| corrupt("invalid endpoint"))?;
    let endpoint = match transport.as_str() {
        "tunnel" => CaptureEndpoint::Tunnel(address),
        "router" => CaptureEndpoint::Router(address),
        _ => return Err(corrupt("invalid transport")),
    };
    let direction: String = row.get(4)?;
    let direction = match direction.as_str() {
        "received" => CaptureDirection::Received,
        "sent" => CaptureDirection::Sent,
        _ => return Err(corrupt("invalid direction")),
    };
    let raw: Vec<u8> = row.get(11)?;
    let frame = CemiFrame::parse(&raw).map_err(|_| corrupt("invalid cEMI frame"))?;
    let expected_group = i64::from(matches!(
        frame.destination_address(),
        knx_rs_core::address::DestinationAddress::Group(_)
    ));
    let fields_match = row.get::<_, i64>(5)? == i64::from(frame.message_code_raw())
        && row.get::<_, i64>(6)? == i64::from(frame.source_address().raw())
        && row.get::<_, i64>(7)? == i64::from(frame.destination_address_raw())
        && row.get::<_, i64>(8)? == expected_group
        && row.get::<_, Option<i64>>(9)? == service_code(&frame)
        && row.get::<_, Vec<u8>>(10)? == frame.payload();
    if !fields_match {
        return Err(corrupt("parsed fields disagree with raw cEMI"));
    }
    Ok(StoredCapture {
        id,
        event: CaptureEvent::from_stored(timestamp, endpoint, direction, frame),
    })
}

#[cfg(test)]
mod tests {
    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;

    use super::*;

    fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).expect("positive fixture")
    }

    fn event(endpoint: CaptureEndpoint, value: u8) -> CaptureEvent {
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(GroupAddress::from_raw(0x0801)),
            Priority::Low,
            &[0x00, 0x80, value],
        );
        CaptureEvent::received(endpoint, frame)
    }

    fn tunnel() -> CaptureEndpoint {
        CaptureEndpoint::Tunnel("192.0.2.1:3671".parse().unwrap())
    }

    #[test]
    fn bounded_history_keeps_monotonic_ids_and_raw_frames() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(2)).unwrap();
        let first = event(tunnel(), 1);
        let first_raw = first.frame().as_bytes().to_vec();
        assert_eq!(store.insert(&first).unwrap(), 1);
        assert_eq!(store.insert(&event(tunnel(), 2)).unwrap(), 2);
        assert_eq!(store.insert(&event(tunnel(), 3)).unwrap(), 3);

        let page = store.read_after(0, nz(2)).unwrap();
        assert_eq!(page.iter().map(|row| row.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(page[0].event.direction(), CaptureDirection::Received);
        assert_eq!(page[1].event.frame().payload(), &[0x00, 0x80, 3]);
        assert!(store.read_after(3, nz(2)).unwrap().is_empty());
        assert_ne!(page[0].event.frame().as_bytes(), first_raw);
        assert!(matches!(
            store.read_after(-1, nz(1)),
            Err(StorageError::InvalidCursor(-1))
        ));
        assert!(matches!(
            store.read_after(0, nz(MAX_PAGE_SIZE + 1)),
            Err(StorageError::PageTooLarge(_))
        ));
    }

    #[test]
    fn restart_replays_exact_frames_and_streams_csv() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let router = CaptureEndpoint::Router("224.0.23.12:3671".parse().unwrap());
        let expected = event(router, 7);
        let raw = expected.frame().as_bytes().to_vec();
        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(writer.insert(&expected).unwrap(), 1);
        drop(writer);

        let mut reader = CaptureStore::open_existing(&path).unwrap();
        let page = reader.read_after(0, nz(1)).unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].id, 1);
        assert_eq!(page[0].event.endpoint(), router);
        assert_eq!(page[0].event.frame().as_bytes(), raw);
        assert!(matches!(
            reader.insert(&expected),
            Err(StorageError::ReadOnly)
        ));
        let mut csv = Vec::new();
        assert_eq!(reader.export_csv(&mut csv, 0).unwrap(), 1);
        let csv = String::from_utf8(csv).unwrap();
        assert!(csv.starts_with("id,observed_at_ms,transport,endpoint,"));
        assert!(
            csv.contains(",router,224.0.23.12:3671,received,1.1.1,1/0/1,GroupValueWrite,0x29,2900")
        );
        drop(reader);

        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(writer.insert(&event(tunnel(), 8)).unwrap(), 2);
        assert_eq!(writer.read_after(1, nz(10)).unwrap()[0].id, 2);
    }

    #[test]
    fn interrupted_transaction_rolls_back_and_preserves_committed_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let mut store = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(store.insert(&event(tunnel(), 1)).unwrap(), 1);
        drop(store);

        let mut connection = Connection::open(&path).unwrap();
        let transaction = connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO capture_events (
                    observed_at_ms, transport, endpoint, direction, message_code,
                    source_raw, destination_raw, destination_is_group, service_code,
                    payload, raw_cemi
                 ) SELECT observed_at_ms, transport, endpoint, direction, message_code,
                          source_raw, destination_raw, destination_is_group, service_code,
                          payload, raw_cemi FROM capture_events WHERE id = 1",
                [],
            )
            .unwrap();
        drop(transaction); // Simulate a writer dying before COMMIT.
        drop(connection);

        let mut store = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(store.read_after(0, nz(10)).unwrap().len(), 1);
        assert_eq!(store.insert(&event(tunnel(), 2)).unwrap(), 2);
    }

    #[test]
    fn rejects_unknown_or_unversioned_schemas_without_rewriting_them() {
        let directory = tempfile::tempdir().unwrap();
        let future = directory.path().join("future.sqlite");
        Connection::open(&future)
            .unwrap()
            .execute_batch("PRAGMA user_version = 99;")
            .unwrap();
        let journal_before: String = Connection::open(&future)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(matches!(
            CaptureStore::open(&future, nz(10)),
            Err(StorageError::UnsupportedSchema(99))
        ));
        let journal_after: String = Connection::open(&future)
            .unwrap()
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal_before, journal_after);
        let unversioned = directory.path().join("unversioned.sqlite");
        Connection::open(&unversioned)
            .unwrap()
            .execute_batch("CREATE TABLE capture_events (unexpected TEXT);")
            .unwrap();
        assert!(matches!(
            CaptureStore::open(&unversioned, nz(10)),
            Err(StorageError::UnversionedCaptureTable)
        ));
        let unrelated = directory.path().join("other.sqlite");
        Connection::open(&unrelated)
            .unwrap()
            .execute_batch("CREATE TABLE user_notes (note TEXT);")
            .unwrap();
        assert!(matches!(
            CaptureStore::open(&unrelated, nz(10)),
            Err(StorageError::UnrecognizedDatabase(table)) if table == "user_notes"
        ));
    }

    #[test]
    fn rejects_corrupt_raw_frame_and_preserves_original_database() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("corrupt.sqlite");
        let mut store = CaptureStore::open(&path, nz(10)).unwrap();
        store.insert(&event(tunnel(), 1)).unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE capture_events SET raw_cemi = X'00' WHERE id = 1",
                [],
            )
            .unwrap();
        drop(connection);

        let store = CaptureStore::open_existing(&path).unwrap();
        assert!(matches!(
            store.read_after(0, nz(1)),
            Err(StorageError::CorruptRow { id: 1, .. })
        ));
        let raw: Vec<u8> = Connection::open(&path)
            .unwrap()
            .query_row(
                "SELECT raw_cemi FROM capture_events WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, [0]);
    }

    #[test]
    fn rejects_parsed_metadata_that_disagrees_with_raw_cemi() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(10)).unwrap();
        store.insert(&event(tunnel(), 1)).unwrap();
        store
            .connection
            .execute("UPDATE capture_events SET source_raw = 0 WHERE id = 1", [])
            .unwrap();
        assert!(matches!(
            store.read_after(0, nz(1)),
            Err(StorageError::CorruptRow { id: 1, .. })
        ));
    }

    #[test]
    fn rejects_pre_epoch_timestamp_without_inserting() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(10)).unwrap();
        let fixture = event(tunnel(), 1);
        let invalid = CaptureEvent::from_stored(
            UNIX_EPOCH - Duration::from_millis(1),
            fixture.endpoint(),
            fixture.direction(),
            fixture.frame().clone(),
        );
        assert!(matches!(
            store.insert(&invalid),
            Err(StorageError::InvalidTimestamp)
        ));
        assert!(store.read_after(0, nz(1)).unwrap().is_empty());
    }

    #[test]
    fn reopening_with_smaller_cap_prunes_without_reusing_ids() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bounded.sqlite");
        let mut store = CaptureStore::open(&path, nz(5)).unwrap();
        for value in 0..5 {
            store.insert(&event(tunnel(), value)).unwrap();
        }
        drop(store);

        let mut store = CaptureStore::open(&path, nz(2)).unwrap();
        assert_eq!(
            store
                .read_after(0, nz(10))
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [4, 5]
        );
        assert_eq!(store.insert(&event(tunnel(), 6)).unwrap(), 6);
    }

    #[test]
    fn export_is_bounded_by_its_starting_high_water_mark() {
        struct AppendingWriter {
            bytes: Vec<u8>,
            writer: CaptureStore,
            appended: bool,
        }

        impl Write for AppendingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if !self.appended {
                    self.writer
                        .insert(&event(tunnel(), 2))
                        .map_err(io::Error::other)?;
                    self.appended = true;
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }

            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("live-export.sqlite");
        let mut initial = CaptureStore::open(&path, nz(10)).unwrap();
        initial.insert(&event(tunnel(), 1)).unwrap();
        drop(initial);
        let reader = CaptureStore::open_existing(&path).unwrap();
        let writer = CaptureStore::open(&path, nz(10)).unwrap();
        let mut output = AppendingWriter {
            bytes: Vec::new(),
            writer,
            appended: false,
        };

        assert_eq!(reader.export_csv(&mut output, 0).unwrap(), 1);
        assert!(output.appended);
        assert_eq!(reader.read_after(0, nz(10)).unwrap().len(), 2);
        assert_eq!(String::from_utf8(output.bytes).unwrap().lines().count(), 2);

        let mut invalid_output = Vec::new();
        assert!(matches!(
            reader.export_csv(&mut invalid_output, -1),
            Err(StorageError::InvalidCursor(-1))
        ));
        assert!(invalid_output.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn new_database_is_private_and_symlink_file_is_rejected() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.sqlite");
        let store = CaptureStore::open(&path, nz(10)).unwrap();
        drop(store);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o077, 0, "database must not be group/world readable");
        assert!(CaptureStore::open_existing(&path).is_ok());

        let link = directory.path().join("linked.sqlite");
        symlink(&path, &link).unwrap();
        assert!(CaptureStore::open_existing(&link).is_err());
        assert!(CaptureStore::open(&link, nz(10)).is_err());
    }
}
