//! flutter_rust_bridge adapter for the Solstice byte ABI.
//!
//! The surface is in [`api`]. `frb_generated` is written by
//! `flutter_rust_bridge_codegen` and is checked in so that building this crate
//! does not require the generator — see `spikes/s2-bridge/generate.sh`.

pub mod api;
mod frb_generated;
