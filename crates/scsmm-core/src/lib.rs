//! Core library of the Rust rewrite of easy-scsmodmanager.
//!
//! Everything that does not need a window lives here: reading SCS mod
//! containers, parsing SII files, locating game installs and profiles,
//! classifying mods and laying out / rewriting the load order.
//!
//! Ported from the Python original (Switch-Bros/easy-scsmodmanager,
//! GPL-3.0-or-later).

pub mod categories;
pub mod classify;
pub mod compat;
pub mod conflicts;
pub mod extract;
pub mod game;
pub mod gamepaths;
pub mod identity;
pub mod load_order;
pub mod manifest;
pub mod presets;
pub mod profile;
pub mod recover;
pub mod scanner;
pub mod scs;
pub mod settings;
pub mod share;
pub mod sii;
pub mod steam;
pub mod workshop;
pub mod workshop_api;

use std::io;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Io(#[from] io::Error),
    #[error("SII parse error: {0}")]
    SiiParse(String),
    #[error("{0}")]
    Format(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("zip: {0}")]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, Error>;

pub(crate) fn format_err(msg: impl Into<String>) -> Error {
    Error::Format(msg.into())
}
