//! `tt-devpro` — settle Dev.Pro time reports from Chrono.
//!
//! Ports `Main.kt`. The clap root mirrors Clikt's `TtCli`; subcommands are wired
//! in as their modules land.

mod config;
mod model;

use clap::Parser;

#[derive(Parser)]
#[command(name = "tt-devpro")]
#[command(about = "Settle Dev.Pro time reports from Chrono")]
struct Cli {}

fn main() {
    let _cli = Cli::parse();
}
