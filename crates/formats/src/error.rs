use std::io;

/// Errors produced while reading Pioneer DJ file formats.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("read of {len} bytes at offset {offset:#x} is out of bounds")]
    OutOfBounds { offset: usize, len: usize },

    #[error("invalid file header: {0}")]
    InvalidHeader(&'static str),

    #[error("page {index} is out of bounds")]
    PageOutOfBounds { index: u32 },

    #[error("page {expected} reports index {found}")]
    PageIndexMismatch { expected: u32, found: u32 },

    #[error("page chain of table starting at page {first_page} loops")]
    PageLoop { first_page: u32 },

    #[error("invalid {section} section at offset {offset:#x}: {reason}")]
    InvalidSection { section: String, offset: usize, reason: &'static str },

    #[error("invalid string kind {kind:#04x} at page offset {offset:#x}")]
    InvalidString { kind: u8, offset: usize },
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
