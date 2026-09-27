// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Bounded, all-or-nothing import of ETS group-address exports.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use knx_rs_core::address::GroupAddress;
use knx_rs_core::dpt::Dpt;
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Maximum accepted ETS export size. Parsing remains bounded even for hostile files.
pub const MAX_ETS_BYTES: usize = 16 * 1024 * 1024;
/// Maximum number of group addresses in one import.
pub const MAX_ETS_GROUPS: usize = 100_000;
const MAX_FIELD_BYTES: usize = 8 * 1024;
const MAX_DPTS_PER_GROUP: usize = 32;
const MAX_XML_RANGE_DEPTH: usize = 32;
const GA_EXPORT_NAMESPACE: &str = "http://knx.org/xml/ga-export/01";

/// Supported ETS group-address export form.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EtsFormat {
    /// Semicolon-separated ETS 3/1 CSV, with four standard or nine extended columns.
    Csv31,
    /// KNX GA Export 01 XML.
    GaXml01,
}

impl EtsFormat {
    /// Stable database and CLI label.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Csv31 => "csv_3_1",
            Self::GaXml01 => "ga_xml_01",
        }
    }
}

/// Text encoding for a CSV export. Latin-1 requires explicit selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CsvEncoding {
    /// Strict UTF-8, optionally with a BOM.
    Utf8,
    /// ISO-8859-1, for legacy ETS exports.
    Latin1,
}

/// One group address and its ETS display metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EtsGroup {
    /// Canonical 16-bit group-address key.
    pub address_raw: u16,
    /// Original ETS notation, retained even for two-level or numeric addresses.
    pub notation: String,
    /// Leaf name.
    pub name: String,
    /// Optional description; standard four-column CSV has none.
    pub description: String,
    /// Ordered parent range names, without the leaf name.
    pub hierarchy: Vec<String>,
    /// Every declared DPT token in export order, without silently selecting one.
    pub dpts: Vec<String>,
}

impl EtsGroup {
    /// Canonical KNX group address used for lookup and wire operations.
    #[must_use]
    pub const fn address(&self) -> GroupAddress {
        GroupAddress::from_raw(self.address_raw)
    }
}

/// Parsed export, ready to replace a persisted catalogue in one transaction.
#[derive(Clone, Debug)]
pub struct EtsCatalog {
    format: EtsFormat,
    groups: BTreeMap<u16, EtsGroup>,
}

impl EtsCatalog {
    /// Parse a file after bounding its byte length.
    ///
    /// # Errors
    ///
    /// Returns a file, size, encoding, structural, or validation error.
    pub fn from_file(
        path: &Path,
        format: EtsFormat,
        encoding: CsvEncoding,
    ) -> Result<Self, EtsError> {
        let file = File::open(path)?;
        let mut bytes = Vec::new();
        file.take(u64::try_from(MAX_ETS_BYTES).map_err(|_| EtsError::FileTooLarge)? + 1)
            .read_to_end(&mut bytes)?;
        Self::from_bytes(&bytes, format, encoding)
    }

    /// Parse bytes with the same bounds used for files and tests.
    ///
    /// # Errors
    ///
    /// Returns an error rather than partially accepting an invalid export.
    pub fn from_bytes(
        bytes: &[u8],
        format: EtsFormat,
        encoding: CsvEncoding,
    ) -> Result<Self, EtsError> {
        if bytes.len() > MAX_ETS_BYTES {
            return Err(EtsError::FileTooLarge);
        }
        let text = match (format, encoding) {
            (EtsFormat::GaXml01, CsvEncoding::Latin1) => {
                return Err(EtsError::UnsupportedXmlEncoding);
            }
            (_, CsvEncoding::Utf8) => {
                std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))?
                    .to_owned()
            }
            (EtsFormat::Csv31, CsvEncoding::Latin1) => {
                bytes.iter().map(|byte| char::from(*byte)).collect()
            }
        };
        let groups = match format {
            EtsFormat::Csv31 => parse_csv(&text)?,
            EtsFormat::GaXml01 => parse_xml(&text)?,
        };
        if groups.is_empty() {
            return Err(EtsError::EmptyCatalog);
        }
        Ok(Self { format, groups })
    }

    /// The source format.
    #[must_use]
    pub const fn format(&self) -> EtsFormat {
        self.format
    }

    /// Number of unique group addresses.
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether this export has no group addresses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// Lookup by the canonical 16-bit address, independent of ETS notation.
    #[must_use]
    pub fn get(&self, address: GroupAddress) -> Option<&EtsGroup> {
        self.groups.get(&address.raw())
    }

    /// Iterate in canonical address order.
    pub fn groups(&self) -> impl Iterator<Item = &EtsGroup> {
        self.groups.values()
    }
}

/// A rejected ETS export.
#[derive(Debug, Error)]
pub enum EtsError {
    /// File-system read failure.
    #[error("ETS file I/O error: {0}")]
    Io(#[from] io::Error),
    /// File exceeds the documented upper bound.
    #[error("ETS export exceeds {MAX_ETS_BYTES} bytes")]
    FileTooLarge,
    /// Too many group addresses.
    #[error("ETS export exceeds {MAX_ETS_GROUPS} group addresses")]
    TooManyGroups,
    /// Invalid UTF-8 in a default-encoded import.
    #[error("ETS export is not valid UTF-8; select Latin-1 explicitly for legacy CSV")]
    Utf8(#[from] std::str::Utf8Error),
    /// Latin-1 is only accepted for CSV.
    #[error("GA Export 01 XML must be UTF-8")]
    UnsupportedXmlEncoding,
    /// XML declaration names a different encoding.
    #[error("GA Export 01 XML declares unsupported encoding: {0}")]
    XmlDeclaredEncoding(String),
    /// CSV lexical parse failure.
    #[error("ETS CSV error: {0}")]
    Csv(#[from] csv::Error),
    /// XML lexical parse failure.
    #[error("ETS XML error: {0}")]
    Xml(#[from] quick_xml::Error),
    /// Malformed or unsupported ETS structure.
    #[error("invalid ETS export: {0}")]
    Structure(String),
    /// Malformed group address.
    #[error("invalid ETS group address: {0}")]
    Address(String),
    /// Two notations refer to the same wire address.
    #[error("duplicate ETS group address: {0}")]
    DuplicateAddress(String),
    /// Empty export after structural rows have been removed.
    #[error("ETS export contains no group addresses")]
    EmptyCatalog,
    /// A declared DPT cannot be understood safely.
    #[error("invalid ETS DPT token: {0}")]
    Dpt(String),
}

/// Parse a declared or explicitly supplied DPT identifier.
///
/// Accepts ETS `DPST-1-1` / `DPT-1-1` and conventional `1.001` forms.
/// Unknown numeric main groups remain representable; runtime encoding checks
/// support separately.
///
/// # Errors
///
/// Returns an error for missing or nonnumeric components.
pub fn parse_dpt(token: &str) -> Result<Dpt, EtsError> {
    let value = token.trim();
    let numeric = value
        .strip_prefix("DPST-")
        .or_else(|| value.strip_prefix("DPT-"))
        .unwrap_or(value);
    let separator = if numeric.contains('.') { '.' } else { '-' };
    let parts: Vec<_> = numeric.split(separator).collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|part| part.is_empty()) {
        return Err(EtsError::Dpt(token.to_owned()));
    }
    let number = |part: &str| {
        part.parse::<u16>()
            .map_err(|_| EtsError::Dpt(token.to_owned()))
    };
    let main = number(parts[0])?;
    let sub = number(parts[1])?;
    let index = if parts.len() == 3 {
        number(parts[2])?
    } else {
        0
    };
    if main == 0 {
        return Err(EtsError::Dpt(token.to_owned()));
    }
    Ok(Dpt::with_index(main, sub, index))
}

fn parse_dpts(raw: &str) -> Result<Vec<String>, EtsError> {
    if raw.trim().is_empty() {
        return Ok(Vec::new());
    }
    let tokens: Vec<String> = raw
        .split(',')
        .map(|token| token.trim().to_owned())
        .collect();
    if tokens.len() > MAX_DPTS_PER_GROUP {
        return Err(EtsError::Structure(
            "too many DPTs on one address".to_owned(),
        ));
    }
    for token in &tokens {
        parse_dpt(token)?;
    }
    Ok(tokens)
}

/// Parse three-level, two-level, decimal, or ETS hexadecimal group-address notation.
///
/// # Errors
///
/// Returns an error for an out-of-range or malformed address.
pub fn parse_group_address(value: &str) -> Result<GroupAddress, EtsError> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('$').or_else(|| value.strip_prefix("0x")) {
        return u16::from_str_radix(hex, 16)
            .map(GroupAddress::from_raw)
            .map_err(|_| EtsError::Address(value.to_owned()));
    }
    if value.bytes().all(|byte| byte.is_ascii_digit()) && !value.is_empty() {
        return value
            .parse::<u16>()
            .map(GroupAddress::from_raw)
            .map_err(|_| EtsError::Address(value.to_owned()));
    }
    value
        .parse()
        .map_err(|_| EtsError::Address(value.to_owned()))
}

fn insert_group(groups: &mut BTreeMap<u16, EtsGroup>, group: EtsGroup) -> Result<(), EtsError> {
    if groups.len() >= MAX_ETS_GROUPS {
        return Err(EtsError::TooManyGroups);
    }
    let notation = group.notation.clone();
    if groups.insert(group.address_raw, group).is_some() {
        return Err(EtsError::DuplicateAddress(notation));
    }
    Ok(())
}

fn check_field(value: &str) -> Result<String, EtsError> {
    if value.len() > MAX_FIELD_BYTES {
        return Err(EtsError::Structure(
            "ETS field exceeds size limit".to_owned(),
        ));
    }
    Ok(value.trim().to_owned())
}

#[expect(
    clippy::too_many_lines,
    reason = "one stateful pass retains CSV hierarchy context"
)]
fn parse_csv(text: &str) -> Result<BTreeMap<u16, EtsGroup>, EtsError> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b';')
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes());
    let mut groups = BTreeMap::new();
    let mut width = None;
    let mut main_name = String::new();
    let mut middle_name = String::new();
    for (index, record) in reader.records().enumerate() {
        let record = record?;
        if record.iter().all(|value| value.trim().is_empty()) {
            continue;
        }
        let columns: Vec<String> = record.iter().map(check_field).collect::<Result<_, _>>()?;
        if width.is_none() {
            if columns.len() != 4 && columns.len() != 9 {
                return Err(EtsError::Structure(
                    "ETS 3/1 CSV needs four or nine columns".to_owned(),
                ));
            }
            width = Some(columns.len());
        }
        if Some(columns.len()) != width {
            return Err(EtsError::Structure(format!(
                "CSV row {} has inconsistent column count",
                index + 1
            )));
        }
        if columns[0].eq_ignore_ascii_case("main")
            && columns[1].eq_ignore_ascii_case("middle")
            && columns[2].eq_ignore_ascii_case("sub")
            && columns[3].eq_ignore_ascii_case("address")
        {
            if index != 0 {
                return Err(EtsError::Structure(
                    "CSV header appears after data".to_owned(),
                ));
            }
            continue;
        }
        let notation = &columns[3];
        if let Some(main) = notation.strip_suffix("/-/-") {
            let parsed: u8 = main
                .parse()
                .map_err(|_| EtsError::Address(notation.clone()))?;
            if parsed > 31 || columns[0].is_empty() {
                return Err(EtsError::Address(notation.clone()));
            }
            main_name.clone_from(&columns[0]);
            middle_name.clear();
            continue;
        }
        if let Some(prefix) = notation.strip_suffix("/-") {
            let parts: Vec<_> = prefix.split('/').collect();
            if parts.len() != 2 || columns[1].is_empty() {
                return Err(EtsError::Address(notation.clone()));
            }
            let main: u8 = parts[0]
                .parse()
                .map_err(|_| EtsError::Address(notation.clone()))?;
            let middle: u8 = parts[1]
                .parse()
                .map_err(|_| EtsError::Address(notation.clone()))?;
            if GroupAddress::new_3level(main, middle, 0).is_err() {
                return Err(EtsError::Address(notation.clone()));
            }
            middle_name.clone_from(&columns[1]);
            continue;
        }
        let address = parse_group_address(notation)?;
        if notation.split('/').count() != 3 || columns[2].is_empty() {
            return Err(EtsError::Structure(format!(
                "CSV row {} is not a named 3-level address",
                index + 1
            )));
        }
        let mut hierarchy = Vec::new();
        let main = if columns[0].is_empty() {
            &main_name
        } else {
            &columns[0]
        };
        let middle = if columns[1].is_empty() {
            &middle_name
        } else {
            &columns[1]
        };
        if !main.is_empty() {
            hierarchy.push(main.clone());
        }
        if !middle.is_empty() {
            hierarchy.push(middle.clone());
        }
        let description = columns.get(6).cloned().unwrap_or_default();
        let dpts = parse_dpts(columns.get(7).map_or("", String::as_str))?;
        insert_group(
            &mut groups,
            EtsGroup {
                address_raw: address.raw(),
                notation: notation.clone(),
                name: columns[2].clone(),
                description,
                hierarchy,
                dpts,
            },
        )?;
    }
    Ok(groups)
}

fn xml_attributes(element: &BytesStart<'_>) -> Result<BTreeMap<String, String>, EtsError> {
    let mut values = BTreeMap::new();
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|error| EtsError::Structure(error.to_string()))?;
        let key = attribute.key.as_ref().to_owned();
        let value = check_field(&attribute.normalized_value(XmlVersion::Implicit1_0)?)?;
        if values.insert(key.clone(), value).is_some() {
            return Err(EtsError::Structure(format!(
                "duplicate XML attribute {key}"
            )));
        }
    }
    Ok(values)
}

#[expect(
    clippy::too_many_lines,
    reason = "one stateful pass enforces XML nesting and source order"
)]
fn parse_xml(text: &str) -> Result<BTreeMap<u16, EtsGroup>, EtsError> {
    let mut reader = Reader::from_str(text);
    let mut buffer = Vec::new();
    let mut groups = BTreeMap::new();
    let mut element_stack: Vec<String> = Vec::new();
    let mut ranges: Vec<String> = Vec::new();
    let mut root_seen = false;
    loop {
        let event = reader.read_event_into(&mut buffer)?;
        match event {
            Event::Decl(declaration) => {
                if let Some(encoding) = declaration.encoding() {
                    let encoding =
                        encoding.map_err(|error| EtsError::Structure(error.to_string()))?;
                    if !encoding.eq_ignore_ascii_case("utf-8") {
                        return Err(EtsError::XmlDeclaredEncoding(encoding.into_owned()));
                    }
                }
            }
            Event::Start(ref element) | Event::Empty(ref element) => {
                let empty = matches!(&event, Event::Empty(_));
                let name = element.local_name().as_ref().to_owned();
                let attrs = xml_attributes(element)?;
                match name.as_str() {
                    "GroupAddress-Export" if !root_seen && element_stack.is_empty() => {
                        if attrs.get("xmlns").map(String::as_str) != Some(GA_EXPORT_NAMESPACE) {
                            return Err(EtsError::Structure(
                                "unsupported GA Export namespace".to_owned(),
                            ));
                        }
                        root_seen = true;
                    }
                    "GroupRange" if root_seen && !element_stack.is_empty() => {
                        if !matches!(
                            element_stack.last().map(String::as_str),
                            Some("GroupAddress-Export" | "GroupRange")
                        ) {
                            return Err(EtsError::Structure(
                                "GroupRange has invalid parent".to_owned(),
                            ));
                        }
                        if ranges.len() >= MAX_XML_RANGE_DEPTH {
                            return Err(EtsError::Structure(
                                "XML range nesting exceeds limit".to_owned(),
                            ));
                        }
                        ranges.push(
                            attrs
                                .get("Name")
                                .ok_or_else(|| {
                                    EtsError::Structure("GroupRange lacks Name".to_owned())
                                })?
                                .clone(),
                        );
                    }
                    "GroupAddress"
                        if element_stack.last().map(String::as_str) == Some("GroupRange") =>
                    {
                        let notation = attrs
                            .get("Address")
                            .ok_or_else(|| {
                                EtsError::Structure("GroupAddress lacks Address".to_owned())
                            })?
                            .clone();
                        let address = parse_group_address(&notation)?;
                        let name = attrs
                            .get("Name")
                            .ok_or_else(|| {
                                EtsError::Structure("GroupAddress lacks Name".to_owned())
                            })?
                            .clone();
                        if name.is_empty() {
                            return Err(EtsError::Structure(
                                "GroupAddress has empty Name".to_owned(),
                            ));
                        }
                        let description = attrs.get("Description").cloned().unwrap_or_default();
                        let dpts = parse_dpts(attrs.get("DPTs").map_or("", String::as_str))?;
                        insert_group(
                            &mut groups,
                            EtsGroup {
                                address_raw: address.raw(),
                                notation,
                                name,
                                description,
                                hierarchy: ranges.clone(),
                                dpts,
                            },
                        )?;
                    }
                    _ => {
                        return Err(EtsError::Structure(format!(
                            "unexpected XML element {name}"
                        )));
                    }
                }
                if empty {
                    if name == "GroupRange" {
                        ranges.pop();
                    }
                } else {
                    element_stack.push(name);
                }
            }
            Event::End(element) => {
                let name = element.local_name().as_ref().to_owned();
                if element_stack.pop().as_deref() != Some(name.as_str()) {
                    return Err(EtsError::Structure("mismatched XML elements".to_owned()));
                }
                if name == "GroupRange" {
                    ranges.pop();
                }
            }
            Event::DocType(_) => {
                return Err(EtsError::Structure("XML DTD is forbidden".to_owned()));
            }
            Event::Eof => break,
            _ => {}
        }
        buffer.clear();
    }
    if !root_seen || !element_stack.is_empty() {
        return Err(EtsError::Structure("incomplete GA Export XML".to_owned()));
    }
    Ok(groups)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CSV_STANDARD: &str = "\"Main\";\"Middle\";\"Sub\";\"Address\"\n\
        \"Lighting\";;;\"1/-/-\"\n\
        ;\"Ground floor\";;\"1/2/-\"\n\
        ;;\"Ceiling; east\";\"1/2/3\"\n";
    const XML_STANDARD: &str = r#"<?xml version="1.0" encoding="utf-8"?>
        <GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01">
          <GroupRange Name="Lighting" RangeStart="1" RangeEnd="2047">
            <GroupRange Name="Ground floor" RangeStart="1" RangeEnd="255">
              <GroupAddress Name="Ceiling &amp; wall" Address="1/2/3"
                Description="Switch &amp; status" DPTs="DPST-1-1,DPST-1-6" />
              <GroupAddress Name="Numeric" Address="0x0A04" DPTs="DPST-9-1" />
            </GroupRange>
          </GroupRange>
        </GroupAddress-Export>"#;

    fn parse_csv_fixture(text: &str) -> Result<EtsCatalog, EtsError> {
        EtsCatalog::from_bytes(text.as_bytes(), EtsFormat::Csv31, CsvEncoding::Utf8)
    }

    fn parse_xml_fixture(text: &str) -> Result<EtsCatalog, EtsError> {
        EtsCatalog::from_bytes(text.as_bytes(), EtsFormat::GaXml01, CsvEncoding::Utf8)
    }

    #[test]
    fn standard_csv_preserves_hierarchy_and_quoted_delimiter() {
        let catalog = parse_csv_fixture(CSV_STANDARD).unwrap();
        assert_eq!(catalog.len(), 1);
        let group = catalog.get("1/2/3".parse().unwrap()).unwrap();
        assert_eq!(group.notation, "1/2/3");
        assert_eq!(group.name, "Ceiling; east");
        assert_eq!(group.hierarchy, ["Lighting", "Ground floor"]);
        assert!(group.description.is_empty());
        assert!(group.dpts.is_empty()); // ETS four-column 3/1 has no DPT column.
    }

    #[test]
    fn extended_csv_retains_description_and_all_dpts() {
        let csv = "Main;Middle;Sub;Address;Central;Unfiltered;Description;DatapointType;Security\n\
            Lighting;Ground floor;Ceiling;1/2/3;true;true;Switch;DPST-1-1, DPST-1-6;none\n";
        let catalog = parse_csv_fixture(csv).unwrap();
        let group = catalog.get("1/2/3".parse().unwrap()).unwrap();
        assert_eq!(group.description, "Switch");
        assert_eq!(group.dpts, ["DPST-1-1", "DPST-1-6"]);
    }

    #[test]
    fn ga_xml_preserves_hierarchy_entities_and_ambiguous_dpts() {
        let catalog = parse_xml_fixture(XML_STANDARD).unwrap();
        assert_eq!(catalog.len(), 2);
        let group = catalog.get("1/2/3".parse().unwrap()).unwrap();
        assert_eq!(group.name, "Ceiling & wall");
        assert_eq!(group.description, "Switch & status");
        assert_eq!(group.hierarchy, ["Lighting", "Ground floor"]);
        assert_eq!(group.dpts, ["DPST-1-1", "DPST-1-6"]);
        let numeric = catalog.get(GroupAddress::from_raw(0x0a04)).unwrap();
        assert_eq!(numeric.notation, "0x0A04");
    }

    #[test]
    fn duplicate_wire_address_fails_even_with_different_notation() {
        let xml = r#"<GroupAddress-Export xmlns="http://knx.org/xml/ga-export/01">
            <GroupRange Name="Main">
              <GroupAddress Name="A" Address="1/2/3" />
              <GroupAddress Name="B" Address="2563" />
            </GroupRange></GroupAddress-Export>"#;
        // 1/2/3 = 0x0a03 = 2563.
        assert!(matches!(
            parse_xml_fixture(xml),
            Err(EtsError::DuplicateAddress(_))
        ));
    }

    #[test]
    fn invalid_address_dpt_and_xml_structure_are_rejected() {
        let invalid_csv = CSV_STANDARD.replace("1/2/3", "1/8/3");
        assert!(matches!(
            parse_csv_fixture(&invalid_csv),
            Err(EtsError::Address(_))
        ));
        let invalid_dpt = XML_STANDARD.replace("DPST-1-6", "not-a-DPT");
        assert!(matches!(
            parse_xml_fixture(&invalid_dpt),
            Err(EtsError::Dpt(_))
        ));
        let dtd = XML_STANDARD.replacen(
            "<GroupAddress-Export",
            "<!DOCTYPE x [<!ENTITY foo SYSTEM 'file:///etc/passwd'>]><GroupAddress-Export",
            1,
        );
        assert!(matches!(
            parse_xml_fixture(&dtd),
            Err(EtsError::Structure(_))
        ));
        let namespace = XML_STANDARD.replace(GA_EXPORT_NAMESPACE, "http://example.invalid");
        assert!(matches!(
            parse_xml_fixture(&namespace),
            Err(EtsError::Structure(_))
        ));
    }

    #[test]
    fn encoding_and_file_bound_are_explicit() {
        let mut latin1 = CSV_STANDARD.replace("Lighting", "Licht").into_bytes();
        latin1.extend_from_slice(b";;\"K\xe4che\";\"1/2/4\"\n");
        assert!(matches!(
            EtsCatalog::from_bytes(&latin1, EtsFormat::Csv31, CsvEncoding::Utf8),
            Err(EtsError::Utf8(_))
        ));
        let legacy =
            EtsCatalog::from_bytes(&latin1, EtsFormat::Csv31, CsvEncoding::Latin1).unwrap();
        assert_eq!(legacy.get("1/2/4".parse().unwrap()).unwrap().name, "Käche");
        assert!(matches!(
            EtsCatalog::from_bytes(
                XML_STANDARD.as_bytes(),
                EtsFormat::GaXml01,
                CsvEncoding::Latin1
            ),
            Err(EtsError::UnsupportedXmlEncoding)
        ));
        assert!(matches!(
            EtsCatalog::from_bytes(
                &vec![b'a'; MAX_ETS_BYTES + 1],
                EtsFormat::Csv31,
                CsvEncoding::Utf8
            ),
            Err(EtsError::FileTooLarge)
        ));
    }

    #[test]
    fn parses_declared_dpt_formats_without_guessing_unknown_groups() {
        assert_eq!(parse_dpt("DPST-9-1").unwrap(), Dpt::new(9, 1));
        assert_eq!(parse_dpt("9.001").unwrap(), Dpt::new(9, 1));
        assert_eq!(parse_dpt("999.001").unwrap(), Dpt::new(999, 1));
        assert!(matches!(parse_dpt("DPT-1"), Err(EtsError::Dpt(_))));
    }
}
