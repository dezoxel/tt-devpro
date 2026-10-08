//! Plan a past range with the live config, Chrono, DevPro and model, and print the plan next
//! to what was actually written to DevPro for the same days. Reads only.
//!
//! `cargo run --example replay -- --from 2026-09-23 --to 2026-10-06`

use chrono::NaiveDate;
use clap::Parser;

#[derive(Parser)]
struct Args {
    /// First day of the range (YYYY-MM-DD).
    #[arg(long)]
    from: NaiveDate,
    /// Last day of the range (YYYY-MM-DD).
    #[arg(long)]
    to: NaiveDate,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    match tt_devpro::replay(args.from, args.to).await {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("✗ Error: {error:#}");
            std::process::exit(1);
        }
    }
}
