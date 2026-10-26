//! Readers for the files rekordbox writes to USB and SD media for Pioneer DJ
//! players.
//!
//! Format knowledge comes from the public analysis by Deep Symmetry
//! (crate-digger); see `PLAN.md` at the workspace root.

pub mod anlz;
mod bytes;
mod error;
pub mod pdb;
pub mod settings;

pub use error::{Error, Result};
