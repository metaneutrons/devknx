// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Versioned SQLite history for raw KNX telegrams.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Write};
use std::net::SocketAddr;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use knx_rs_core::address::GroupAddress;
use knx_rs_core::cemi::CemiFrame;
use knx_rs_ip::RoutingLostMessage;
use rusqlite::{
    Connection, MAIN_DB, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use serde::Serialize;
use thiserror::Error;

use crate::capture::{CaptureDirection, CaptureEndpoint, CaptureEvent, RoutingLossEvent};
use crate::ets::{EtsCatalog, EtsGroup, parse_dpt, parse_group_address};

const SCHEMA_VERSION: i64 = 4;
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

const SCHEMA_V2: &str = "
    CREATE TABLE routing_loss_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        observed_at_ms INTEGER NOT NULL CHECK (observed_at_ms >= 0),
        endpoint TEXT NOT NULL,
        source TEXT NOT NULL,
        device_state INTEGER NOT NULL CHECK (device_state BETWEEN 0 AND 255),
        lost_messages INTEGER NOT NULL CHECK (lost_messages BETWEEN 0 AND 65535)
    );
    CREATE INDEX idx_routing_loss_time ON routing_loss_events(observed_at_ms);
    PRAGMA user_version = 2;
";

const SCHEMA_V3: &str = "
    CREATE TABLE ets_imports (
        revision INTEGER PRIMARY KEY AUTOINCREMENT,
        imported_at_ms INTEGER NOT NULL CHECK (imported_at_ms >= 0),
        format TEXT NOT NULL CHECK (format IN ('csv_3_1', 'ga_xml_01'))
    );
    CREATE TABLE ets_groups (
        revision INTEGER NOT NULL,
        address_raw INTEGER NOT NULL CHECK (address_raw BETWEEN 0 AND 65535),
        entry_json TEXT NOT NULL,
        PRIMARY KEY (revision, address_raw)
    );
    CREATE INDEX idx_ets_groups_address ON ets_groups(address_raw, revision DESC);
    PRAGMA user_version = 3;
";

const SCHEMA_V4: &str = "
    CREATE TABLE operation_audit (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
        origin TEXT NOT NULL,
        kind TEXT NOT NULL CHECK (kind IN ('read', 'typed_write', 'raw_write')),
        address_raw INTEGER NOT NULL CHECK (address_raw BETWEEN 0 AND 65535),
        dpt TEXT,
        raw_cemi BLOB NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('started', 'transmitted', 'failed')),
        detail TEXT
    );
    CREATE INDEX idx_operation_audit_time ON operation_audit(started_at_ms);
    PRAGMA user_version = 4;
";

/// Errors opening, writing, reading, or exporting capture history.
#[derive(Debug, Error)]
pub enum StorageError {
    /// SQLite returned an error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// A persisted ETS entry could not be serialized or decoded.
    #[error("ETS metadata JSON error: {0}")]
    Json(#[from] serde_json::Error),
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
    /// Another process already owns the writable capture store.
    #[error("capture writer is already active for {0}")]
    WriterBusy(PathBuf),
    /// A writable capture directory must not be writable by other users.
    #[error("capture directory is writable by another user: {0}")]
    InsecureDirectory(PathBuf),
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
    /// A stored router diagnostic is invalid.
    #[error("corrupt routing-loss row {id}: {reason}")]
    CorruptRoutingLoss {
        /// Diagnostic ID.
        id: i64,
        /// Validation failure.
        reason: String,
    },
    /// Router diagnostics must be associated with a multicast endpoint.
    #[error("routing loss requires a router endpoint")]
    InvalidRouterEndpoint,
    /// A persisted ETS entry is inconsistent with its canonical key.
    #[error(
        "corrupt ETS group address at revision {revision}, raw address {address_raw}: {reason}"
    )]
    CorruptEtsGroup {
        /// Import revision.
        revision: i64,
        /// Canonical group-address key.
        address_raw: u16,
        /// Validation failure.
        reason: String,
    },
    /// An audit row was missing or already finalized.
    #[error("operation audit row {0} is missing or already finalized")]
    CorruptOperationAudit(i64),
}

/// One durable capture with its monotonic database ID.
#[derive(Debug)]
pub struct StoredCapture {
    /// Monotonic ID, never reused after retention pruning.
    pub id: i64,
    /// The raw-preserving capture event.
    pub event: CaptureEvent,
}

/// One durable router-reported loss diagnostic with a monotonic ID.
#[derive(Debug)]
pub struct StoredRoutingLoss {
    /// Monotonic ID within the router-loss history.
    pub id: i64,
    /// Source, device state and reported routing-frame loss count.
    pub event: RoutingLossEvent,
}

/// One durable operation attempt, including an explicit raw/typed distinction.
#[derive(Debug, Serialize)]
pub struct OperationAuditEntry {
    /// Monotonic audit cursor.
    pub id: i64,
    /// Unix millisecond timestamp before attempted transmission.
    pub started_at_ms: i64,
    /// Interface that requested the operation.
    pub origin: String,
    /// `read`, `typed_write`, or `raw_write`.
    pub kind: String,
    /// Canonical group address.
    pub address_raw: u16,
    /// Chosen DPT for typed writes.
    pub dpt: Option<String>,
    /// Exact prepared cEMI bytes.
    pub raw_cemi: String,
    /// `started`, `transmitted`, or `failed`.
    pub status: String,
    /// Transport failure if one occurred.
    pub detail: Option<String>,
}

/// Single-owner SQLite capture store.
pub struct CaptureStore {
    connection: Connection,
    max_events: Option<NonZeroU32>,
    writable: bool,
    _writer_lease: Option<File>,
}

impl CaptureStore {
    /// Create or migrate a writable database and apply the retention cap.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O failure, an unsupported schema, or migration failure.
    pub fn open(path: &Path, max_events: NonZeroU32) -> Result<Self, StorageError> {
        Self::open_writable(path, Some(max_events))
    }

    /// Open a writable store for metadata replacement without pruning capture history.
    /// The same single-writer lease applies; stop `serve` before importing.
    ///
    /// # Errors
    ///
    /// Returns an error for I/O, unsupported schema, or an active writer.
    pub fn open_for_ets_import(path: &Path) -> Result<Self, StorageError> {
        Self::open_writable(path, None)
    }

    fn open_writable(path: &Path, max_events: Option<NonZeroU32>) -> Result<Self, StorageError> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)?;
        }
        let database_path = normalized_sqlite_path(path)?;
        let writer_lease = acquire_writer_lease(&database_path)?;
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
        if let Some(cap) = max_events {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            prune(&transaction, cap)?;
            transaction.commit()?;
        }
        Ok(Self {
            connection,
            max_events,
            writable: true,
            _writer_lease: writer_lease,
        })
    }

    /// Replace the active ETS catalogue in one transaction while retaining prior revisions.
    /// Capture rows and their raw cEMI bytes are never modified.
    ///
    /// # Errors
    ///
    /// Returns a read-only, SQLite, or JSON serialization error.
    pub fn import_ets(&mut self, catalog: &EtsCatalog) -> Result<i64, StorageError> {
        if !self.writable {
            return Err(StorageError::ReadOnly);
        }
        let now_ms = unix_now_ms()?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO ets_imports (imported_at_ms, format) VALUES (?1, ?2)",
            params![now_ms, catalog.format().as_str()],
        )?;
        let revision = transaction.last_insert_rowid();
        for group in catalog.groups() {
            transaction.execute(
                "INSERT INTO ets_groups (revision, address_raw, entry_json) VALUES (?1, ?2, ?3)",
                params![
                    revision,
                    i64::from(group.address_raw),
                    serde_json::to_string(group)?
                ],
            )?;
        }
        transaction.commit()?;
        Ok(revision)
    }

    /// Current ETS import revision, if a catalogue has been imported.
    ///
    /// # Errors
    ///
    /// Returns a SQLite query error.
    pub fn ets_revision(&self) -> Result<Option<i64>, StorageError> {
        if schema_version(&self.connection)? < 3 {
            return Ok(None);
        }
        Ok(self
            .connection
            .query_row("SELECT MAX(revision) FROM ets_imports", [], |row| {
                row.get(0)
            })?)
    }

    /// Look up the active ETS metadata by canonical group address.
    ///
    /// # Errors
    ///
    /// Returns a SQLite or corrupt-metadata error.
    pub fn ets_group(&self, address: GroupAddress) -> Result<Option<EtsGroup>, StorageError> {
        let Some(revision) = self.ets_revision()? else {
            return Ok(None);
        };
        let raw = address.raw();
        let json: Option<String> = self
            .connection
            .query_row(
                "SELECT entry_json FROM ets_groups WHERE revision = ?1 AND address_raw = ?2",
                params![revision, i64::from(raw)],
                |row| row.get(0),
            )
            .optional()?;
        let Some(json) = json else {
            return Ok(None);
        };
        let corrupt = |reason: String| StorageError::CorruptEtsGroup {
            revision,
            address_raw: raw,
            reason,
        };
        let group: EtsGroup =
            serde_json::from_str(&json).map_err(|error| corrupt(error.to_string()))?;
        if group.address_raw != raw
            || parse_group_address(&group.notation)
                .map_err(|error| corrupt(error.to_string()))?
                .raw()
                != raw
            || group.dpts.iter().any(|dpt| parse_dpt(dpt).is_err())
        {
            return Err(corrupt(
                "stored fields disagree with canonical address or DPT declaration".to_owned(),
            ));
        }
        Ok(Some(group))
    }

    /// Durably record a prepared operation before transmission. A crash leaves
    /// the row as `started`, never as a false success.
    ///
    /// # Errors
    ///
    /// Returns an error if this store is read-only or the audit write fails.
    pub fn start_operation_audit(
        &mut self,
        origin: &str,
        kind: &str,
        address_raw: u16,
        dpt: Option<&str>,
        raw_cemi: &[u8],
    ) -> Result<i64, StorageError> {
        if !self.writable {
            return Err(StorageError::ReadOnly);
        }
        self.connection.execute(
            "INSERT INTO operation_audit (started_at_ms, origin, kind, address_raw, dpt, raw_cemi, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'started')",
            params![unix_now_ms()?, origin, kind, i64::from(address_raw), dpt, raw_cemi],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Mark a durable operation attempt as transmitted or failed.
    ///
    /// # Errors
    ///
    /// Returns an error if the audit row cannot be updated.
    pub fn finish_operation_audit(
        &mut self,
        id: i64,
        transmitted: bool,
        detail: Option<&str>,
    ) -> Result<(), StorageError> {
        if !self.writable {
            return Err(StorageError::ReadOnly);
        }
        let status = if transmitted { "transmitted" } else { "failed" };
        let updated = self.connection.execute(
            "UPDATE operation_audit SET status = ?1, detail = ?2 WHERE id = ?3 AND status = 'started'",
            params![status, detail, id],
        )?;
        if updated != 1 {
            return Err(StorageError::CorruptOperationAudit(id));
        }
        Ok(())
    }

    /// Read a bounded page of operation attempts after an exclusive cursor.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor, oversized page, or SQLite failure.
    pub fn read_operation_audit_after(
        &self,
        after: i64,
        limit: NonZeroU32,
    ) -> Result<Vec<OperationAuditEntry>, StorageError> {
        if after < 0 {
            return Err(StorageError::InvalidCursor(after));
        }
        if limit.get() > MAX_PAGE_SIZE {
            return Err(StorageError::PageTooLarge(limit.get()));
        }
        if schema_version(&self.connection)? < 4 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT id, started_at_ms, origin, kind, address_raw, dpt, raw_cemi, status, detail
             FROM operation_audit WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after, limit.get()], |row| {
            let raw: Vec<u8> = row.get(6)?;
            let address_raw: i64 = row.get(4)?;
            let address_raw = u16::try_from(address_raw)
                .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(4, address_raw))?;
            Ok(OperationAuditEntry {
                id: row.get(0)?,
                started_at_ms: row.get(1)?,
                origin: row.get(2)?,
                kind: row.get(3)?,
                address_raw,
                dpt: row.get(5)?,
                raw_cemi: hex(&raw),
                status: row.get(7)?,
                detail: row.get(8)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
    }

    /// Open an existing database for history reads without creating or migrating it.
    ///
    /// # Errors
    ///
    /// Returns an error if the file is absent, unreadable, or has an unsupported schema.
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
            writable: false,
            _writer_lease: None,
        })
    }

    /// Create a consistent, standalone SQLite snapshot without overwriting a file.
    ///
    /// SQLite's online backup API includes committed WAL transactions while a
    /// capture writer is active. The temporary snapshot is installed only after
    /// backup succeeds; an existing destination remains untouched.
    ///
    /// # Errors
    ///
    /// Returns an error if the backup cannot be made or the destination exists.
    pub fn backup_to(&self, destination: &Path) -> Result<(), StorageError> {
        let parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let temporary = tempfile::NamedTempFile::new_in(parent)?;
        self.connection.backup(MAIN_DB, temporary.path(), None)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(destination)
            .map_err(|error| StorageError::Io(error.error))?;
        Ok(())
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

    /// Atomically append a router-reported loss and prune its separate history.
    ///
    /// # Errors
    ///
    /// Returns an error if the store is read-only, the endpoint is not a router,
    /// or the transaction fails.
    pub fn insert_routing_loss(&mut self, event: &RoutingLossEvent) -> Result<i64, StorageError> {
        let max_events = self.max_events.ok_or(StorageError::ReadOnly)?;
        let CaptureEndpoint::Router(endpoint) = event.endpoint() else {
            return Err(StorageError::InvalidRouterEndpoint);
        };
        let observed_at_ms = i64::try_from(
            event
                .observed_at()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| StorageError::InvalidTimestamp)?
                .as_millis(),
        )
        .map_err(|_| StorageError::InvalidTimestamp)?;
        let report = event.report();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO routing_loss_events (
                observed_at_ms, endpoint, source, device_state, lost_messages
            ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                observed_at_ms,
                endpoint.to_string(),
                report.source.to_string(),
                i64::from(report.device_state),
                i64::from(report.lost_messages),
            ],
        )?;
        let id = transaction.last_insert_rowid();
        prune(&transaction, max_events)?;
        transaction.commit()?;
        Ok(id)
    }

    /// Read router-loss diagnostics strictly after a separate monotonic cursor.
    ///
    /// A read-only schema-v1 store has no diagnostics table and returns an
    /// empty page; opening it for writing migrates it to v2.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid bounds, SQLite failure, or corrupt data.
    pub fn read_routing_losses_after(
        &self,
        cursor: i64,
        limit: NonZeroU32,
    ) -> Result<Vec<StoredRoutingLoss>, StorageError> {
        if cursor < 0 {
            return Err(StorageError::InvalidCursor(cursor));
        }
        if limit.get() > MAX_PAGE_SIZE {
            return Err(StorageError::PageTooLarge(limit.get()));
        }
        if schema_version(&self.connection)? == 1 {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT id, observed_at_ms, endpoint, source, device_state, lost_messages
             FROM routing_loss_events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let mut rows = statement.query(params![cursor, limit.get()])?;
        let mut reports = Vec::new();
        while let Some(row) = rows.next()? {
            reports.push(decode_routing_loss_row(row)?);
        }
        Ok(reports)
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

    /// Read the most recent bounded capture page, returned in chronological order.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized page, SQLite failure, or corrupt data.
    pub fn read_latest(&self, limit: NonZeroU32) -> Result<Vec<StoredCapture>, StorageError> {
        if limit.get() > MAX_PAGE_SIZE {
            return Err(StorageError::PageTooLarge(limit.get()));
        }
        let mut statement = self.connection.prepare(
            "SELECT id, observed_at_ms, transport, endpoint, direction, message_code,
                    source_raw, destination_raw, destination_is_group, service_code,
                    payload, raw_cemi
             FROM capture_events ORDER BY id DESC LIMIT ?1",
        )?;
        let mut rows = statement.query(params![limit.get()])?;
        let mut captures = Vec::new();
        while let Some(row) = rows.next()? {
            captures.push(decode_row(row)?);
        }
        captures.reverse();
        Ok(captures)
    }

    /// Read captures before an exclusive ID, nearest first but returned in
    /// chronological order for an interactive history window.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid cursor, oversized page, SQLite failure,
    /// or corrupt data.
    pub fn read_before(
        &self,
        cursor: i64,
        limit: NonZeroU32,
    ) -> Result<Vec<StoredCapture>, StorageError> {
        if cursor <= 0 {
            return Err(StorageError::InvalidCursor(cursor));
        }
        if limit.get() > MAX_PAGE_SIZE {
            return Err(StorageError::PageTooLarge(limit.get()));
        }
        let mut statement = self.connection.prepare(
            "SELECT id, observed_at_ms, transport, endpoint, direction, message_code,
                    source_raw, destination_raw, destination_is_group, service_code,
                    payload, raw_cemi
             FROM capture_events WHERE id < ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let mut rows = statement.query(params![cursor, limit.get()])?;
        let mut captures = Vec::new();
        while let Some(row) = rows.next()? {
            captures.push(decode_row(row)?);
        }
        captures.reverse();
        Ok(captures)
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

fn acquire_writer_lease(database_path: &Path) -> Result<Option<File>, StorageError> {
    if database_path == Path::new(":memory:") {
        return Ok(None);
    }
    let mut lease_name = database_path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing database filename"))?
        .to_os_string();
    lease_name.push(".writer.lock");
    let lease_path = database_path.with_file_name(lease_name);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

        let parent = database_path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "missing parent directory")
        })?;
        if std::fs::metadata(parent)?.permissions().mode() & 0o022 != 0 {
            return Err(StorageError::InsecureDirectory(parent.to_path_buf()));
        }
        match std::fs::symlink_metadata(&lease_path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "capture writer lock is not a regular file",
                )
                .into());
            }
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(false).mode(0o600);
        let file = options.open(&lease_path)?;
        lock_writer_file(file, lease_path)
    }
    #[cfg(not(unix))]
    {
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lease_path)?;
        lock_writer_file(file, lease_path)
    }
}

fn lock_writer_file(file: File, path: PathBuf) -> Result<Option<File>, StorageError> {
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Err(StorageError::WriterBusy(path)),
        Err(TryLockError::Error(error)) => Err(error.into()),
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
            transaction.execute_batch(SCHEMA_V2)?;
            transaction.execute_batch(SCHEMA_V3)?;
            transaction.execute_batch(SCHEMA_V4)?;
            transaction.commit()?;
            Ok(())
        }
        1 => {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCHEMA_V2)?;
            transaction.execute_batch(SCHEMA_V3)?;
            transaction.execute_batch(SCHEMA_V4)?;
            transaction.commit()?;
            Ok(())
        }
        2 => {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCHEMA_V3)?;
            transaction.execute_batch(SCHEMA_V4)?;
            transaction.commit()?;
            Ok(())
        }
        3 => {
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            transaction.execute_batch(SCHEMA_V4)?;
            transaction.commit()?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        other => Err(StorageError::UnsupportedSchema(other)),
    }
}

fn require_schema(connection: &Connection) -> Result<(), StorageError> {
    let version = schema_version(connection)?;
    if (1..=SCHEMA_VERSION).contains(&version) {
        Ok(())
    } else {
        Err(StorageError::UnsupportedSchema(version))
    }
}

fn schema_version(connection: &Connection) -> Result<i64, StorageError> {
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn unix_now_ms() -> Result<i64, StorageError> {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageError::InvalidTimestamp)?
            .as_millis(),
    )
    .map_err(|_| StorageError::InvalidTimestamp)
}

fn prune(transaction: &Transaction<'_>, max_events: NonZeroU32) -> Result<(), StorageError> {
    transaction.execute(
        "DELETE FROM capture_events WHERE id <= (
            SELECT id FROM capture_events ORDER BY id DESC LIMIT 1 OFFSET ?1
        )",
        [max_events.get()],
    )?;
    transaction.execute(
        "DELETE FROM routing_loss_events WHERE id <= (
            SELECT id FROM routing_loss_events ORDER BY id DESC LIMIT 1 OFFSET ?1
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

fn decode_routing_loss_row(row: &rusqlite::Row<'_>) -> Result<StoredRoutingLoss, StorageError> {
    let id: i64 = row.get(0)?;
    let corrupt = |reason: &str| StorageError::CorruptRoutingLoss {
        id,
        reason: reason.to_owned(),
    };
    let observed_at_ms: i64 = row.get(1)?;
    let observed_at = u64::try_from(observed_at_ms)
        .ok()
        .and_then(|ms| UNIX_EPOCH.checked_add(Duration::from_millis(ms)))
        .ok_or_else(|| corrupt("invalid timestamp"))?;
    let endpoint: String = row.get(2)?;
    let endpoint: SocketAddr = endpoint.parse().map_err(|_| corrupt("invalid endpoint"))?;
    if !endpoint.ip().is_multicast() {
        return Err(corrupt("endpoint is not multicast"));
    }
    let source: String = row.get(3)?;
    let source: SocketAddr = source.parse().map_err(|_| corrupt("invalid source"))?;
    let device_state: i64 = row.get(4)?;
    let device_state = u8::try_from(device_state).map_err(|_| corrupt("invalid device state"))?;
    let lost_messages: i64 = row.get(5)?;
    let lost_messages =
        u16::try_from(lost_messages).map_err(|_| corrupt("invalid lost-message count"))?;
    Ok(StoredRoutingLoss {
        id,
        event: RoutingLossEvent::from_stored(
            observed_at,
            CaptureEndpoint::Router(endpoint),
            RoutingLostMessage {
                source,
                device_state,
                lost_messages,
            },
        ),
    })
}

#[cfg(test)]
mod tests {
    use knx_rs_core::address::{DestinationAddress, GroupAddress, IndividualAddress};
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;

    use super::*;
    use crate::ets::{CsvEncoding, EtsFormat};

    fn ets_fixture(name: &str) -> EtsCatalog {
        let xml = format!(
            "<GroupAddress-Export xmlns=\"http://knx.org/xml/ga-export/01\"><GroupRange Name=\"Main\"><GroupAddress Name=\"{name}\" Address=\"1/2/3\" Description=\"Fixture\" DPTs=\"DPST-1-1,DPST-1-6\" /></GroupRange></GroupAddress-Export>"
        );
        EtsCatalog::from_bytes(xml.as_bytes(), EtsFormat::GaXml01, CsvEncoding::Utf8).unwrap()
    }

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

    fn routing_loss(count: u16) -> RoutingLossEvent {
        RoutingLossEvent::received(
            CaptureEndpoint::Router("224.0.23.12:3671".parse().unwrap()),
            RoutingLostMessage {
                source: "192.0.2.2:3671".parse().unwrap(),
                device_state: 3,
                lost_messages: count,
            },
        )
    }

    #[test]
    fn v1_migrates_to_v4_without_losing_captures() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let mut original = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(original.insert(&event(tunnel(), 7)).unwrap(), 1);
        drop(original);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "DROP INDEX idx_operation_audit_time;
                 DROP TABLE operation_audit;
                 DROP INDEX idx_ets_groups_address;
                 DROP TABLE ets_groups;
                 DROP TABLE ets_imports;
                 DROP INDEX idx_routing_loss_time;
                 DROP TABLE routing_loss_events;
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        drop(connection);

        let legacy = CaptureStore::open_existing(&path).unwrap();
        assert!(
            legacy
                .read_routing_losses_after(0, nz(10))
                .unwrap()
                .is_empty()
        );
        drop(legacy);

        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(schema_version(&writer.connection).unwrap(), SCHEMA_VERSION);
        assert_eq!(writer.insert(&event(tunnel(), 1)).unwrap(), 2);
        assert_eq!(writer.insert_routing_loss(&routing_loss(258)).unwrap(), 1);
        drop(writer);

        let reader = CaptureStore::open_existing(&path).unwrap();
        let captures = reader.read_after(0, nz(10)).unwrap();
        assert_eq!(captures.len(), 2);
        assert_eq!(captures[0].event.frame().payload(), &[0x00, 0x80, 7]);
        let reports = reader.read_routing_losses_after(0, nz(10)).unwrap();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].id, 1);
        assert_eq!(reports[0].event.report().lost_messages, 258);
        assert_eq!(reports[0].event.report().device_state, 3);
    }

    #[test]
    fn failed_v1_migration_rolls_back_without_touching_history() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("legacy.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection.execute_batch(SCHEMA_V1).unwrap();
        connection
            .execute_batch("CREATE TABLE routing_loss_events (incompatible TEXT);")
            .unwrap();
        drop(connection);

        assert!(matches!(
            CaptureStore::open(&path, nz(10)),
            Err(StorageError::Sqlite(_))
        ));
        let connection = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 1);
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM capture_events", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(CaptureStore::open_existing(&path).is_ok());
    }

    #[test]
    fn routing_losses_have_separate_bounded_monotonic_history() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(2)).unwrap();
        assert_eq!(store.insert(&event(tunnel(), 1)).unwrap(), 1);
        for count in 1..=3 {
            assert_eq!(
                store.insert_routing_loss(&routing_loss(count)).unwrap(),
                i64::from(count)
            );
        }
        let reports = store.read_routing_losses_after(0, nz(10)).unwrap();
        assert_eq!(reports.iter().map(|row| row.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(reports[0].event.report().lost_messages, 2);
        assert_eq!(store.read_after(0, nz(10)).unwrap().len(), 1);
        assert!(matches!(
            store.read_routing_losses_after(-1, nz(1)),
            Err(StorageError::InvalidCursor(-1))
        ));
        assert!(matches!(
            store.read_routing_losses_after(0, nz(MAX_PAGE_SIZE + 1)),
            Err(StorageError::PageTooLarge(_))
        ));
        assert!(matches!(
            store.insert_routing_loss(&RoutingLossEvent::received(
                tunnel(),
                routing_loss(1).report(),
            )),
            Err(StorageError::InvalidRouterEndpoint)
        ));
    }

    #[test]
    fn corrupt_routing_loss_does_not_decode_as_valid() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(10)).unwrap();
        store.insert_routing_loss(&routing_loss(5)).unwrap();
        store
            .connection
            .execute(
                "UPDATE routing_loss_events SET source = 'not-an-address' WHERE id = 1",
                [],
            )
            .unwrap();
        assert!(matches!(
            store.read_routing_losses_after(0, nz(10)),
            Err(StorageError::CorruptRoutingLoss { id: 1, .. })
        ));
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
    fn latest_page_is_bounded_and_chronological() {
        let mut store = CaptureStore::open(Path::new(":memory:"), nz(10)).unwrap();
        for value in 1..=5 {
            store.insert(&event(tunnel(), value)).unwrap();
        }
        assert_eq!(
            store
                .read_latest(nz(3))
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [3, 4, 5]
        );
        assert_eq!(
            store
                .read_before(3, nz(2))
                .unwrap()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(matches!(
            store.read_before(0, nz(2)),
            Err(StorageError::InvalidCursor(0))
        ));
        assert!(matches!(
            store.read_latest(nz(MAX_PAGE_SIZE + 1)),
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
    fn writable_store_has_one_owner_and_releases_its_lease() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let first = CaptureStore::open(&path, nz(10)).unwrap();
        assert!(matches!(
            CaptureStore::open(&path, nz(10)),
            Err(StorageError::WriterBusy(_))
        ));
        assert!(CaptureStore::open_existing(&path).is_ok());
        drop(first);
        assert!(CaptureStore::open(&path, nz(10)).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn writer_lease_rejects_symlink_and_shared_directory() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim");
        std::fs::write(&victim, b"untouched").unwrap();
        let path = directory.path().join("captures.sqlite");
        symlink(
            &victim,
            directory.path().join("captures.sqlite.writer.lock"),
        )
        .unwrap();
        assert!(matches!(
            CaptureStore::open(&path, nz(10)),
            Err(StorageError::Io(error)) if error.kind() == io::ErrorKind::InvalidInput
        ));
        assert_eq!(std::fs::read(&victim).unwrap(), b"untouched");

        let shared = directory.path().join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            CaptureStore::open(&shared.join("captures.sqlite"), nz(10)),
            Err(StorageError::InsecureDirectory(_))
        ));
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
    fn empty_schema_zero_database_migrates_to_current_version() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("empty.sqlite");
        let connection = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&connection).unwrap(), 0);
        drop(connection);

        let mut store = CaptureStore::open(&path, nz(10)).unwrap();
        assert_eq!(schema_version(&store.connection).unwrap(), SCHEMA_VERSION);
        assert_eq!(store.insert(&event(tunnel(), 7)).unwrap(), 1);
        drop(store);
        assert_eq!(
            CaptureStore::open_existing(&path)
                .unwrap()
                .read_after(0, nz(10))
                .unwrap()[0]
                .event
                .frame()
                .payload(),
            &[0x00, 0x80, 7]
        );
    }

    #[test]
    fn ets_import_revisions_preserve_raw_captures_and_backup_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let backup = directory.path().join("backup.sqlite");
        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        writer.insert(&event(tunnel(), 42)).unwrap();
        drop(writer);

        let mut importer = CaptureStore::open_for_ets_import(&path).unwrap();
        assert_eq!(importer.import_ets(&ets_fixture("First")).unwrap(), 1);
        assert_eq!(importer.import_ets(&ets_fixture("Second")).unwrap(), 2);
        let address = "1/2/3".parse().unwrap();
        assert_eq!(importer.ets_group(address).unwrap().unwrap().name, "Second");
        assert_eq!(
            importer.read_after(0, nz(10)).unwrap()[0]
                .event
                .frame()
                .payload(),
            &[0, 0x80, 42]
        );
        importer.backup_to(&backup).unwrap();
        drop(importer);

        let mut reader = CaptureStore::open_existing(&backup).unwrap();
        assert_eq!(reader.ets_revision().unwrap(), Some(2));
        assert_eq!(
            reader.ets_group(address).unwrap().unwrap().dpts,
            ["DPST-1-1", "DPST-1-6"]
        );
        assert!(matches!(
            reader.import_ets(&ets_fixture("Third")),
            Err(StorageError::ReadOnly)
        ));
        let revisions: i64 = reader
            .connection
            .query_row("SELECT COUNT(*) FROM ets_imports", [], |row| row.get(0))
            .unwrap();
        assert_eq!(revisions, 2);
    }

    #[test]
    fn v2_migration_and_failed_ets_import_leave_capture_history_intact() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("captures.sqlite");
        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        writer.insert(&event(tunnel(), 7)).unwrap();
        drop(writer);
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("DROP INDEX idx_operation_audit_time; DROP TABLE operation_audit; DROP INDEX idx_ets_groups_address; DROP TABLE ets_groups; DROP TABLE ets_imports; PRAGMA user_version = 2;").unwrap();
        drop(legacy);
        assert_eq!(
            CaptureStore::open_existing(&path)
                .unwrap()
                .ets_revision()
                .unwrap(),
            None
        );

        let mut importer = CaptureStore::open_for_ets_import(&path).unwrap();
        assert_eq!(
            schema_version(&importer.connection).unwrap(),
            SCHEMA_VERSION
        );
        let address = "1/2/3".parse().unwrap();
        assert_eq!(importer.import_ets(&ets_fixture("Good")).unwrap(), 1);
        importer.connection.execute_batch("CREATE TRIGGER reject_second_ets_group BEFORE INSERT ON ets_groups WHEN NEW.revision = 2 BEGIN SELECT RAISE(ABORT, 'fixture rollback'); END;").unwrap();
        assert!(matches!(
            importer.import_ets(&ets_fixture("Rejected")),
            Err(StorageError::Sqlite(_))
        ));
        assert_eq!(importer.ets_revision().unwrap(), Some(1));
        assert_eq!(importer.ets_group(address).unwrap().unwrap().name, "Good");
        assert_eq!(importer.read_after(0, nz(10)).unwrap().len(), 1);
    }

    #[test]
    fn v3_audit_migration_preserves_ets_and_rolls_back_on_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("v3.sqlite");
        let mut writer = CaptureStore::open(&path, nz(10)).unwrap();
        writer.insert(&event(tunnel(), 11)).unwrap();
        writer.import_ets(&ets_fixture("Preserved")).unwrap();
        drop(writer);
        let legacy = Connection::open(&path).unwrap();
        legacy.execute_batch("DROP INDEX idx_operation_audit_time; DROP TABLE operation_audit; PRAGMA user_version = 3;").unwrap();
        drop(legacy);

        let migrated = CaptureStore::open_for_ets_import(&path).unwrap();
        assert_eq!(
            schema_version(&migrated.connection).unwrap(),
            SCHEMA_VERSION
        );
        assert_eq!(
            migrated
                .ets_group("1/2/3".parse().unwrap())
                .unwrap()
                .unwrap()
                .name,
            "Preserved"
        );
        assert_eq!(migrated.read_after(0, nz(10)).unwrap().len(), 1);
        drop(migrated);

        let conflicting = Connection::open(&path).unwrap();
        conflicting.execute_batch("DROP INDEX idx_operation_audit_time; DROP TABLE operation_audit; CREATE TABLE operation_audit (marker TEXT); PRAGMA user_version = 3;").unwrap();
        drop(conflicting);
        assert!(matches!(
            CaptureStore::open_for_ets_import(&path),
            Err(StorageError::Sqlite(_))
        ));
        let check = Connection::open(&path).unwrap();
        assert_eq!(schema_version(&check).unwrap(), 3);
        let marker: i64 = check
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('operation_audit') WHERE name = 'marker'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, 1);
        let revision: i64 = check
            .query_row("SELECT MAX(revision) FROM ets_imports", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revision, 1);
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
