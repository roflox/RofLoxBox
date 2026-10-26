//! Bounds-checked little-endian reads from byte slices.

use crate::error::{Error, Result};

pub(crate) fn slice(buf: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    offset
        .checked_add(len)
        .and_then(|end| buf.get(offset..end))
        .ok_or(Error::OutOfBounds { offset, len })
}

pub(crate) fn u8_at(buf: &[u8], offset: usize) -> Result<u8> {
    Ok(slice(buf, offset, 1)?[0])
}

pub(crate) fn u16_at(buf: &[u8], offset: usize) -> Result<u16> {
    let b = slice(buf, offset, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

pub(crate) fn u32_at(buf: &[u8], offset: usize) -> Result<u32> {
    let b = slice(buf, offset, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Reads a 24-bit little-endian integer.
pub(crate) fn u24_at(buf: &[u8], offset: usize) -> Result<u32> {
    let b = slice(buf, offset, 3)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], 0]))
}

pub(crate) fn u16_be_at(buf: &[u8], offset: usize) -> Result<u16> {
    let b = slice(buf, offset, 2)?;
    Ok(u16::from_be_bytes([b[0], b[1]]))
}

pub(crate) fn u32_be_at(buf: &[u8], offset: usize) -> Result<u32> {
    let b = slice(buf, offset, 4)?;
    Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}
