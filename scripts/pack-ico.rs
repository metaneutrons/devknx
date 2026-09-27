// SPDX-License-Identifier: GPL-3.0-only
// Copyright (C) 2026 Fabian Schmieder

//! Package PNG variants in a Windows ICO container without changing pixels.

use std::env;
use std::fs;
use std::io::{self, Write};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let output = args.next().ok_or("usage: pack-ico OUTPUT IMAGE...")?;
    let images: Vec<_> = args.collect();
    if images.is_empty() || images.len() > u16::MAX as usize {
        return Err("expected between 1 and 65535 images".into());
    }

    let mut entries = Vec::with_capacity(images.len());
    for path in images {
        let bytes = fs::read(&path)?;
        let size = png_size(&bytes)?;
        if size.0 != size.1 || !matches!(size.0, 16 | 32 | 48 | 256) {
            return Err(format!("unsupported icon size in {path}: {size:?}").into());
        }
        entries.push((size.0, bytes));
    }

    let mut file = fs::File::create(output)?;
    file.write_all(&0_u16.to_le_bytes())?;
    file.write_all(&1_u16.to_le_bytes())?;
    file.write_all(&(entries.len() as u16).to_le_bytes())?;

    let mut offset = 6_u32 + 16_u32 * entries.len() as u32;
    for (size, bytes) in &entries {
        file.write_all(&[if *size == 256 { 0 } else { *size as u8 }])?;
        file.write_all(&[if *size == 256 { 0 } else { *size as u8 }])?;
        file.write_all(&[0, 0])?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&32_u16.to_le_bytes())?;
        file.write_all(&(bytes.len() as u32).to_le_bytes())?;
        file.write_all(&offset.to_le_bytes())?;
        offset = offset.checked_add(bytes.len() as u32).ok_or("ICO too large")?;
    }
    for (_, bytes) in entries {
        file.write_all(&bytes)?;
    }
    Ok(())
}

fn png_size(bytes: &[u8]) -> io::Result<(u32, u32)> {
    if bytes.len() < 24 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "not a PNG"));
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().unwrap());
    let height = u32::from_be_bytes(bytes[20..24].try_into().unwrap());
    Ok((width, height))
}
