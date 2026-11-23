//! Rust engine for iPhone Photo Vault.
//!
//! It serves the same UI and the same API as the Python engine (see docs/engine-openapi.json), starting
//! with settings, drives and folders, status, and the review and trip screens. Device access, copying,
//! jobs and filing are still in the Python engine; those routes say so instead of pretending.
mod afc;
mod analysis;
mod app;
mod backup;
mod config;
mod copier;
mod device;
mod edits;
mod filing;
mod layout;
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
#[command(name = "photovault-engine", about = "Rust engine for iPhone Photo Vault")]
struct Cli {
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
}

fn random_token() -> String {
    (0..3).map(|_| format!("{:016x}", RandomState::new().build_hasher().finish())).collect()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let Cli { cmd: Cmd::Ui { port, token, .. } } = Cli::parse();
    let token = token.unwrap_or_else(random_token);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("photovault Rust engine: http://127.0.0.1:{port}/  (Ctrl+C to stop)");
    axum::serve(listener, app::router(token)).await?;
    Ok(())
}
