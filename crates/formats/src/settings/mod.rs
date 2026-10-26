//! Player and mixer settings: `PIONEER/DEVSETTING.DAT`, `MYSETTING.DAT`,
//! `MYSETTING2.DAT` and `DJMMYSETTING.DAT`.
//!
//! Little-endian. Layout: `u32` string area length (0x60), three 32-byte
//! NUL-padded strings (brand, software, version), `u32` data length, the
//! data, a CRC-16/XMODEM checksum and two zero bytes.
//!
//! The data blocks are decoded into [`PlayerSettings`], [`Player2Settings`],
//! [`DeviceSettings`] and [`MixerSettings`]. Field offsets were found by
//! comparing files that differ in a single option (`tests/fixtures/settings`).

mod typed;
pub mod values;

pub use typed::{DeviceSettings, MixerSettings, Player2Settings, PlayerSettings};

use std::path::Path;

use crate::bytes::{slice, u16_at, u32_at};
use crate::error::{Error, Result};

const STRING_AREA_LEN: usize = 0x60;
const STRING_LEN: usize = 0x20;
const DATA_START: usize = 0x68;
/// Leading bytes of every data block except `MYSETTING2.DAT`'s.
const DATA_MAGIC: [u8; 4] = [0x78, 0x56, 0x34, 0x12];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsKind {
    /// `DEVSETTING.DAT`, 32 data bytes.
    Device,
    /// `MYSETTING.DAT`, 40 data bytes.
    Player,
    /// `MYSETTING2.DAT`, 40 data bytes without the leading magic.
    Player2,
    /// `DJMMYSETTING.DAT`, 52 data bytes.
    Mixer,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsFile {
    pub brand: String,
    pub software: String,
    pub version: String,
    pub kind: SettingsKind,
    pub data: Vec<u8>,
    pub checksum: u16,
    /// Everything before the checksum, which the mixer file's checksum covers.
    prefix: Vec<u8>,
}

impl SettingsFile {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_bytes(&std::fs::read(path)?)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        if u32_at(bytes, 0)? as usize != STRING_AREA_LEN {
            return Err(Error::InvalidHeader("unexpected string area length"));
        }
        let string = |index: usize| -> Result<String> {
            let raw = slice(bytes, 4 + index * STRING_LEN, STRING_LEN)?;
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            Ok(raw[..end].iter().map(|&b| char::from(b)).collect())
        };
        let data_len = u32_at(bytes, DATA_START - 4)? as usize;
        let data = slice(bytes, DATA_START, data_len)?.to_vec();
        let checksum_at = DATA_START + data_len;
        let has_magic = data.starts_with(&DATA_MAGIC);
        let kind = match (data_len, has_magic) {
            (32, true) => SettingsKind::Device,
            (40, true) => SettingsKind::Player,
            (40, false) => SettingsKind::Player2,
            (52, true) => SettingsKind::Mixer,
            _ => SettingsKind::Unknown,
        };
        Ok(Self {
            brand: string(0)?,
            software: string(1)?,
            version: string(2)?,
            kind,
            checksum: u16_at(bytes, checksum_at)?,
            prefix: bytes[..checksum_at].to_vec(),
            data,
        })
    }

    pub fn settings(&self) -> Option<Settings> {
        Some(match self.kind {
            SettingsKind::Device => Settings::Device(DeviceSettings::from_data(&self.data)),
            SettingsKind::Player => Settings::Player(PlayerSettings::from_data(&self.data)),
            SettingsKind::Player2 => Settings::Player2(Player2Settings::from_data(&self.data)),
            SettingsKind::Mixer => Settings::Mixer(MixerSettings::from_data(&self.data)),
            SettingsKind::Unknown => return None,
        })
    }

    /// A settings file with rekordbox's default values for `kind`.
    ///
    /// Panics for [`SettingsKind::Unknown`].
    pub fn with_defaults(kind: SettingsKind) -> Self {
        let (brand, version, data): (&str, &str, &[u8]) = match kind {
            SettingsKind::Device => ("PIONEER DJ", "6.6.1", &DEFAULT_DEVICE),
            SettingsKind::Player => ("PIONEER", "0.001", &DEFAULT_PLAYER),
            SettingsKind::Player2 => ("PIONEER", "0.001", &DEFAULT_PLAYER2),
            SettingsKind::Mixer => ("PioneerDJ", "1.000", &DEFAULT_MIXER),
            SettingsKind::Unknown => panic!("no defaults for unknown settings"),
        };
        let mut file = Self {
            brand: brand.to_owned(),
            software: "rekordbox".to_owned(),
            version: version.to_owned(),
            kind,
            data: data.to_vec(),
            checksum: 0,
            prefix: Vec::new(),
        };
        file.checksum = file.compute_checksum();
        file
    }

    /// Replaces the data block's fields with `settings`. Bytes the typed view
    /// does not cover keep their current values.
    pub fn set_settings(&mut self, settings: &Settings) {
        let kind = match settings {
            Settings::Device(s) => {
                s.write_into(&mut self.data);
                SettingsKind::Device
            }
            Settings::Player(s) => {
                s.write_into(&mut self.data);
                SettingsKind::Player
            }
            Settings::Player2(s) => {
                s.write_into(&mut self.data);
                SettingsKind::Player2
            }
            Settings::Mixer(s) => {
                s.write_into(&mut self.data);
                SettingsKind::Mixer
            }
        };
        assert_eq!(kind, self.kind, "settings of another kind");
        self.checksum = self.compute_checksum();
    }

    /// Serializes the file with a freshly computed checksum.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = self.prefix_bytes();
        out.extend(self.compute_checksum().to_le_bytes());
        out.extend([0, 0]);
        out
    }

    fn prefix_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(DATA_START + self.data.len() + 4);
        out.extend((STRING_AREA_LEN as u32).to_le_bytes());
        for text in [&self.brand, &self.software, &self.version] {
            let mut field = [0u8; STRING_LEN];
            let bytes: Vec<u8> = text.chars().map(|c| u8::try_from(c).unwrap_or(b'?')).collect();
            let len = bytes.len().min(STRING_LEN - 1);
            field[..len].copy_from_slice(&bytes[..len]);
            out.extend(field);
        }
        out.extend((self.data.len() as u32).to_le_bytes());
        out.extend(&self.data);
        out
    }

    fn compute_checksum(&self) -> u16 {
        match self.kind {
            SettingsKind::Mixer => crc16_xmodem(&self.prefix_bytes()),
            _ => crc16_xmodem(&self.data),
        }
    }

    /// Whether the stored checksum matches the contents. Player and device
    /// files checksum the data only; mixer files checksum everything before
    /// the checksum.
    pub fn checksum_is_valid(&self) -> bool {
        let covered = match self.kind {
            SettingsKind::Mixer => &self.prefix[..],
            _ => &self.data[..],
        };
        crc16_xmodem(covered) == self.checksum
    }
}

// Default data blocks as written by rekordbox for a new export.
const DEFAULT_DEVICE: [u8; 32] = [
    0x78, 0x56, 0x34, 0x12, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const DEFAULT_PLAYER: [u8; 40] = [
    0x78, 0x56, 0x34, 0x12, 0x02, 0x00, 0x00, 0x00, 0x81, 0x83, 0x81, 0x88, 0x81, 0x01, 0x82, 0x81,
    0x81, 0x01, 0x01, 0x01, 0x82, 0x80, 0x80, 0x81, 0x80, 0x81, 0x80, 0x00, 0x00, 0x81, 0x00, 0x00,
    0x81, 0x81, 0x81, 0x80, 0x81, 0x80, 0x00, 0x00,
];
const DEFAULT_PLAYER2: [u8; 40] = [
    0x81, 0x80, 0x83, 0x83, 0x81, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x81, 0x85, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];
const DEFAULT_MIXER: [u8; 52] = [
    0x78, 0x56, 0x34, 0x12, 0x01, 0x00, 0x00, 0x00, 0x20, 0x00, 0x00, 0x00, 0x81, 0x82, 0x80, 0x80,
    0x81, 0x81, 0x80, 0x81, 0x80, 0x80, 0x85, 0x82, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// The decoded contents of a settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settings {
    Device(DeviceSettings),
    Player(PlayerSettings),
    Player2(Player2Settings),
    Mixer(MixerSettings),
}

/// CRC-16/XMODEM: polynomial 0x1021, initial value 0, no reflection.
pub fn crc16_xmodem(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |mut crc, &byte| {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 { (crc << 1) ^ 0x1021 } else { crc << 1 };
        }
        crc
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc16_xmodem_check_value() {
        // Standard check value for the ASCII string "123456789".
        assert_eq!(super::crc16_xmodem(b"123456789"), 0x31c3);
    }
}
