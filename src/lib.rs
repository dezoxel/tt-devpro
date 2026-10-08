#![deny(warnings)]
#![deny(clippy::all)]
//! `tt-devpro` — settle Dev.Pro time reports from Chrono.
//!
//! A library with a thin binary in front of it, rather than a binary alone, so the
//! replay example can build plans from the same code the binary runs. The public
//! surface is kept to what those two callers use: everything else stays private,
//! which keeps `dead_code` able to see an item nothing calls any more.

mod api;
mod cli;
mod commands;
mod config;
mod cookie;
mod fmt;
mod model;
mod service;

pub use cli::run;
