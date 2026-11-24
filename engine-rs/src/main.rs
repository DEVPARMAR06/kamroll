//! Rust engine for iPhone Photo Vault.
//!
//! It serves the same UI and the same API as the Python engine (see docs/engine-openapi.json) and has the same
//! command line: backup, sync, verify, adopt, import and compare old folders.
mod adopt;
mod afc;
mod ai;
mod analysis;
mod app;
mod backup;
mod cli;
mod config;
mod copier;
mod device;
mod edits;
mod filing;
mod layout;
mod legacy;
mod manifest;
mod metadata;
mod schema;
mod system;
mod tagging;
mod thumbs;
mod trips;
mod verify;

use clap::{Parser, Subcommand};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};

#[derive(Parser)]
#[command(name = "photovault-engine", about = "Rust engine for iPhone Photo Vault (the app's engine and the photovault command line)")]
struct Cli {
    /// path to config.toml
    #[arg(short = 'c', long = "config", global = true)]
    config: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Serve the app and its API on 127.0.0.1
    Ui {
        #[arg(long, default_value_t = 8765)]
        port: u16,
        /// Accepted for compatibility with the Python engine: this engine never opens a browser itself.
        #[arg(long)]
        no_open: bool,
        #[arg(long)]
        token: Option<String>,
    },
    /// scan the phone and copy everything new
    Run {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        only_reencoded_originals: bool,
    },
    /// copy adopted files from the old folder into the backup layout
    Consolidate {
        #[arg(long)]
        limit: Option<usize>,
    },
    /// record phone files in the manifest, copy nothing
    Scan,
    /// mark phone files already present in an existing backup folder as done
    Adopt { path: String },
    /// show manifest counts and where capture dates came from
    Status,
    /// check backed-up files on the SSD (no phone needed)
    Verify {
        #[arg(long)]
        deep: bool,
    },
    /// move files imported into FromOldBackup/ into the normal dated folders
    RelocateOld,
    /// copy every file of an old backup folder into the vault (read-only on the old folder)
    ImportOld {
        path: String,
        #[arg(long)]
        limit: Option<usize>,
    },
    /// classify what an old backup folder really contains, vs the phone
    CompareOld {
        path: String,
        #[arg(long)]
        out: Option<String>,
    },
    /// compare the phone with the SSD and report whether they are in sync
    Sync,
}

fn random_token() -> String {
    (0..3).map(|_| format!("{:016x}", RandomState::new().build_hasher().finish())).collect()
}

fn main() {
    let Cli { config, cmd } = Cli::parse();
    if let Some(c) = config {
        std::env::set_var("PHOTOVAULT_CONFIG", c);
    }
    let blank = |cmd: &str| cli::Args { cmd: cmd.into(), path: None, limit: None, deep: false, dry_run: false, only_reencoded: false, out: None };
    let args = match cmd {
        Cmd::Ui { port, token, .. } => {
            let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
            let r: anyhow::Result<()> = rt.block_on(async move {
                let token = token.unwrap_or_else(random_token);
                let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
                println!("photovault Rust engine: http://127.0.0.1:{port}/  (Ctrl+C to stop)");
                axum::serve(listener, app::router(token)).await?;
                Ok(())
            });
            if let Err(e) = r {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::Run { dry_run, limit, only_reencoded_originals } => cli::Args { dry_run, limit, only_reencoded: only_reencoded_originals, ..blank("run") },
        Cmd::Consolidate { limit } => cli::Args { limit, ..blank("consolidate") },
        Cmd::Scan => blank("scan"),
        Cmd::Adopt { path } => cli::Args { path: Some(path), ..blank("adopt") },
        Cmd::Status => blank("status"),
        Cmd::Verify { deep } => cli::Args { deep, ..blank("verify") },
        Cmd::RelocateOld => blank("relocate-old"),
        Cmd::ImportOld { path, limit } => cli::Args { path: Some(path), limit, ..blank("import-old") },
        Cmd::CompareOld { path, out } => cli::Args { path: Some(path), out, ..blank("compare-old") },
        Cmd::Sync => blank("sync"),
    };
    std::process::exit(cli::run(args));
}
