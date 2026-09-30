// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Additive, versioned interpretation of raw capture messages.

use knx_rs_core::cemi::CemiFrame;
use knx_rs_core::dpt;
use knx_rs_core::message::ApduType;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::ets::{parse_dpt, parse_group_address};
use crate::ipc::IpcMessage;
use crate::storage::{CaptureStore, StorageError};

/// Schema version of [`CaptureEnrichment`].
pub const CAPTURE_ENRICHMENT_SCHEMA_VERSION: u8 = 1;

/// Derived capture metadata, kept separate from the immutable raw capture event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureEnrichment {
    /// Version of this additive enrichment schema.
    pub schema_version: u8,
    /// Active ETS import revision used for this lookup, if one exists.
    pub ets_revision: Option<i64>,
    /// ETS leaf name for the destination group address.
    pub group_name: Option<String>,
    /// Ordered ETS parent range names, without the leaf name.
    pub hierarchy: Vec<String>,
    /// Every DPT declared by ETS, preserving export order and duplicates.
    pub dpts: Vec<String>,
    /// Decoded value when the service and one declared DPT allow an unambiguous decode.
    pub value: Option<String>,
}

impl Default for CaptureEnrichment {
    fn default() -> Self {
        Self {
            schema_version: CAPTURE_ENRICHMENT_SCHEMA_VERSION,
            ets_revision: None,
            group_name: None,
            hierarchy: Vec::new(),
            dpts: Vec::new(),
            value: None,
        }
    }
}

/// Failure while enriching a capture message.
#[derive(Debug, Error)]
pub enum EnrichmentError {
    /// The supplied IPC message is not a capture.
    #[error("only capture IPC messages can be enriched")]
    NotCapture,
    /// The active ETS revision or group metadata could not be read.
    #[error(transparent)]
    Storage(#[from] StorageError),
    /// JSON serialization failed or produced an unexpected capture shape.
    #[error("capture enrichment could not be serialized")]
    Serialization,
}

/// Derive current ETS metadata and a safe display value from a raw capture.
///
/// The returned object is separate from `message`; no persisted or transmitted
/// capture field is modified. Unknown destinations and missing ETS metadata keep
/// their raw capture fields but produce no group information.
///
/// # Errors
///
/// Returns [`EnrichmentError::NotCapture`] for a non-capture IPC message, or a
/// storage error if the ETS catalog cannot be read.
pub fn enrich_capture(
    message: &IpcMessage,
    store: &CaptureStore,
) -> Result<CaptureEnrichment, EnrichmentError> {
    let IpcMessage::Capture {
        destination,
        service,
        raw_cemi,
        ..
    } = message
    else {
        return Err(EnrichmentError::NotCapture);
    };

    enrich_fields(destination, service, raw_cemi, store)
}

fn enrich_fields(
    destination: &str,
    service: &str,
    raw_cemi: &str,
    store: &CaptureStore,
) -> Result<CaptureEnrichment, EnrichmentError> {
    let mut enrichment = CaptureEnrichment {
        ets_revision: store.ets_revision()?,
        ..CaptureEnrichment::default()
    };

    if let Ok(address) = parse_group_address(destination)
        && let Some(group) = store.ets_group(address)?
    {
        enrichment.group_name = Some(group.name);
        enrichment.hierarchy = group.hierarchy;
        enrichment.dpts = group.dpts;
    }

    enrichment.value = decode_capture_value(raw_cemi, &enrichment.dpts, service);
    Ok(enrichment)
}

/// Enrich a matching group-value response returned by a group read.
///
/// Returns `None` if the supplied frame is malformed or is not a group-value
/// response. The caller must still retain the raw transport receipt.
///
/// # Errors
///
/// Returns an ETS storage error while resolving the response destination.
pub fn enrich_response_frame(
    raw_cemi: &str,
    store: &CaptureStore,
) -> Result<Option<CaptureEnrichment>, EnrichmentError> {
    let Some(bytes) = decode_hex(raw_cemi) else {
        return Ok(None);
    };
    let Ok(frame) = CemiFrame::parse(&bytes) else {
        return Ok(None);
    };
    if frame
        .tpdu()
        .and_then(|tpdu| tpdu.apdu().map(|apdu| apdu.apdu_type))
        != Some(ApduType::GroupValueResponse)
    {
        return Ok(None);
    }
    enrich_fields(
        &frame.destination_address().to_string(),
        "Response",
        raw_cemi,
        store,
    )
    .map(Some)
}

/// Render a capture as its existing raw JSON fields plus additive enrichment.
///
/// # Errors
///
/// Returns a non-capture or ETS storage error from [`enrich_capture`].
pub fn enriched_capture_json(
    message: &IpcMessage,
    store: &CaptureStore,
) -> Result<Value, EnrichmentError> {
    let enrichment = enrich_capture(message, store)?;
    let mut value = serde_json::to_value(message).map_err(|_| EnrichmentError::Serialization)?;
    let object = value
        .as_object_mut()
        .ok_or(EnrichmentError::Serialization)?;
    object.insert(
        "enrichment".into(),
        serde_json::to_value(enrichment).map_err(|_| EnrichmentError::Serialization)?,
    );
    Ok(value)
}

/// Extract the untyped data bytes from a group-value write or response.
///
/// Short values are returned without the APCI bits. Read requests, other
/// services and malformed frames have no group-value payload.
#[must_use]
pub fn raw_group_value(raw_cemi: &str) -> Option<Vec<u8>> {
    let bytes = decode_hex(raw_cemi)?;
    let frame = CemiFrame::parse(&bytes).ok()?;
    let tpdu = frame.tpdu()?;
    let apdu = tpdu.apdu()?;
    matches!(
        apdu.apdu_type,
        ApduType::GroupValueWrite | ApduType::GroupValueResponse
    )
    .then(|| apdu.data.clone())
}

fn decode_capture_value(raw_cemi: &str, dpts: &[String], service: &str) -> Option<String> {
    if service == "Read" {
        return Some("Read request".into());
    }
    if !matches!(service, "Write" | "Response")
        || dpts.len() != 1
        || !raw_cemi.len().is_multiple_of(2)
    {
        return None;
    }
    let bytes = decode_hex(raw_cemi)?;
    let frame = CemiFrame::parse(&bytes).ok()?;
    let apdu = frame.tpdu()?.apdu()?.clone();
    let dpt = parse_dpt(&dpts[0]).ok()?;
    dpt::decode(dpt, &apdu.data)
        .ok()
        .map(|value| value.to_string())
}

fn decode_hex(raw_cemi: &str) -> Option<Vec<u8>> {
    if !raw_cemi.len().is_multiple_of(2) {
        return None;
    }
    raw_cemi
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        })
        .collect::<Option<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use knx_rs_core::address::{DestinationAddress, IndividualAddress};
    use knx_rs_core::cemi::CemiFrame;
    use knx_rs_core::message::MessageCode;
    use knx_rs_core::types::Priority;

    use crate::ets::{CsvEncoding, EtsCatalog, EtsFormat};

    use super::*;

    fn store_with_catalog() -> (tempfile::TempDir, CaptureStore) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("capture.sqlite");
        let mut store = CaptureStore::open(&path, NonZeroU32::new(10).unwrap()).unwrap();
        let catalog = EtsCatalog::from_bytes(
            br#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01"><GroupRange Name="Lighting"><GroupAddress Name="Valid" Address="1/2/3" DPTs="DPT-1-1"/><GroupAddress Name="Ambiguous" Address="1/2/4" DPTs="DPT-1-1,DPT-5-1"/><GroupAddress Name="Unknown DPT" Address="1/2/5" DPTs="DPT-999-1"/></GroupRange></GroupAddress-Export>"#,
            EtsFormat::GaXml01,
            CsvEncoding::Utf8,
        )
        .unwrap();
        store.import_ets(&catalog).unwrap();
        (directory, store)
    }

    fn capture(destination: &str, service: &str, data: &[u8]) -> IpcMessage {
        let address = parse_group_address(destination).unwrap();
        let frame = CemiFrame::new_l_data(
            MessageCode::LDataInd,
            IndividualAddress::from_raw(0x1101),
            DestinationAddress::Group(address),
            Priority::Low,
            data,
        );
        let mut raw_cemi = String::new();
        for byte in frame.as_bytes() {
            use std::fmt::Write as _;
            write!(raw_cemi, "{byte:02x}").unwrap();
        }
        IpcMessage::Capture {
            id: Some(1),
            observed_at_ms: 1,
            endpoint: "tunnel://192.0.2.1:3671".into(),
            direction: "received".into(),
            source: "1.1.1".into(),
            destination: destination.into(),
            service: service.into(),
            raw_cemi,
        }
    }

    #[test]
    fn raw_group_values_preserve_payload_without_guessing_a_dpt() {
        for (data, expected) in [
            (vec![0, 0x81], Some(vec![1])),
            (vec![0, 0x80, 0x0c, 0x56], Some(vec![0x0c, 0x56])),
            (vec![0, 0x40, 0xff], Some(vec![0xff])),
            (vec![0, 0], None),
        ] {
            let IpcMessage::Capture { raw_cemi, .. } = capture("1/2/3", "Write", &data) else {
                unreachable!()
            };
            assert_eq!(raw_group_value(&raw_cemi), expected);
        }
        assert_eq!(raw_group_value("2900"), None);
        assert_eq!(raw_group_value("zz"), None);
    }

    #[test]
    fn enrichment_includes_revision_and_hierarchy_and_decodes_one_declared_dpt() {
        let (_directory, store) = store_with_catalog();
        let message = capture("1/2/3", "Write", &[0, 0x80, 1]);
        let before = message.clone();

        let enrichment = enrich_capture(&message, &store).unwrap();

        assert_eq!(enrichment.schema_version, CAPTURE_ENRICHMENT_SCHEMA_VERSION);
        assert_eq!(enrichment.ets_revision, Some(1));
        assert_eq!(enrichment.group_name.as_deref(), Some("Valid"));
        assert_eq!(enrichment.hierarchy, ["Lighting"]);
        assert_eq!(enrichment.dpts, ["DPT-1-1"]);
        assert_eq!(enrichment.value.as_deref(), Some("true"));
        assert_eq!(
            message, before,
            "enrichment must not change the raw capture"
        );
        let rendered = enriched_capture_json(&message, &store).unwrap();
        assert_eq!(
            rendered["raw_cemi"],
            serde_json::to_value(&message).unwrap()["raw_cemi"]
        );
        assert_eq!(rendered["enrichment"]["group_name"], "Valid");
        assert_eq!(rendered["enrichment"]["schema_version"], 1);
    }

    #[test]
    fn unknown_and_ambiguous_dpts_never_produce_a_decoded_value() {
        let (_directory, store) = store_with_catalog();

        let unknown = enrich_capture(&capture("1/2/5", "Write", &[0, 0x80, 1]), &store).unwrap();
        assert_eq!(unknown.group_name.as_deref(), Some("Unknown DPT"));
        assert_eq!(unknown.dpts, ["DPT-999-1"]);
        assert_eq!(unknown.value, None);

        let ambiguous = enrich_capture(&capture("1/2/4", "Write", &[0, 0x80, 1]), &store).unwrap();
        assert_eq!(ambiguous.group_name.as_deref(), Some("Ambiguous"));
        assert_eq!(ambiguous.dpts, ["DPT-1-1", "DPT-5-1"]);
        assert_eq!(ambiguous.value, None);
    }

    #[test]
    fn read_response_enrichment_requires_an_actual_group_response() {
        let (_directory, store) = store_with_catalog();
        let response = capture("1/2/3", "Response", &[0, 0x40, 1]);
        let IpcMessage::Capture { raw_cemi, .. } = response else {
            unreachable!()
        };
        let enriched = enrich_response_frame(&raw_cemi, &store).unwrap().unwrap();
        assert_eq!(enriched.group_name.as_deref(), Some("Valid"));
        assert_eq!(enriched.value.as_deref(), Some("true"));
        assert_eq!(enrich_response_frame("not hex", &store).unwrap(), None);
        let write = capture("1/2/3", "Write", &[0, 0x80, 1]);
        let IpcMessage::Capture { raw_cemi, .. } = write else {
            unreachable!()
        };
        assert_eq!(enrich_response_frame(&raw_cemi, &store).unwrap(), None);
    }

    #[test]
    fn read_request_is_described_without_selecting_a_dpt() {
        let (_directory, store) = store_with_catalog();
        let enrichment = enrich_capture(&capture("1/2/4", "Read", &[0, 0]), &store).unwrap();

        assert_eq!(enrichment.dpts, ["DPT-1-1", "DPT-5-1"]);
        assert_eq!(enrichment.value.as_deref(), Some("Read request"));
    }

    #[test]
    fn unknown_group_keeps_the_active_revision_without_group_metadata() {
        let (_directory, store) = store_with_catalog();
        let enrichment =
            enrich_capture(&capture("1/2/99", "Write", &[0, 0x80, 1]), &store).unwrap();

        assert_eq!(enrichment.ets_revision, Some(1));
        assert_eq!(enrichment.group_name, None);
        assert!(enrichment.hierarchy.is_empty());
        assert!(enrichment.dpts.is_empty());
        assert_eq!(enrichment.value, None);
    }
}
