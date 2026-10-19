//! xcomd — xcom session daemon (M0: spike harness under `xcomd m0`).

use anyhow::Result;
use clap::{Parser, Subcommand};

mod m0;

#[derive(Parser)]
#[command(name = "xcomd", about = "xcom session daemon", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// M0 spike harness (capture pipeline, latency, input injection).
    M0(m0::Args),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::M0(args) => m0::run(args),
    }
}
