//! xcomd — xcom session daemon (M0: spike harness under `xcomd m0`).

use anyhow::Result;
use clap::{Parser, Subcommand};

#[cfg(feature = "m0")]
mod m0;
mod control;
mod serve;

#[derive(Parser)]
#[command(name = "xcomd", about = "xcom session daemon", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve one client over the dedicated secure channel (legacy/direct mode).
    Serve(serve::ServeArgs),
    /// Run a session owner that outlives its clients (started by `attach`).
    Session(serve::SessionArgs),
    /// Attach to (or create) a session and print its handshake line.
    Attach(control::AttachArgs),
    /// List the sessions running for this user.
    Sessions,
    /// End a session.
    Kill(control::KillArgs),
    /// M0 spike harness (capture pipeline, latency, input injection).
    #[cfg(feature = "m0")]
    M0(m0::Args),
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve(args) => serve::run(args),
        Cmd::Session(args) => serve::run_session(args),
        Cmd::Attach(args) => control::attach(&args),
        Cmd::Sessions => control::list(),
        Cmd::Kill(args) => control::kill(&args.session),
        #[cfg(feature = "m0")]
        Cmd::M0(args) => m0::run(args),
    }
}
