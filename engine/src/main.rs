//! The Kamroll command line: back up, sync, verify, adopt, import and compare old folders.
use clap::{Parser, Subcommand};
use kamroll_engine::CommandArgs;

#[derive(Parser)]
#[command(name = "kamroll-engine", about = "Kamroll: back up an iPhone to an SSD at original quality (the app's engine and the command line)")]
struct Cli {
    /// path to config.toml
    #[arg(short = 'c', long = "config", global = true)]
    config: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Development only: serve the app and its API on 127.0.0.1 (the desktop app does not use this)
    #[cfg(feature = "web")]
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

fn main() {
    let Cli { config, cmd } = Cli::parse();
    if let Some(c) = config {
        std::env::set_var("PHOTOVAULT_CONFIG", c);
    }
    kamroll_engine::migrate_settings();
    let blank = |cmd: &str| CommandArgs { cmd: cmd.into(), path: None, limit: None, deep: false, dry_run: false, only_reencoded: false, out: None };
    let args = match cmd {
        #[cfg(feature = "web")]
        Cmd::Ui { port, token, .. } => {
            if let Err(e) = kamroll_engine::serve_web(port, token) {
                eprintln!("{e}");
                std::process::exit(1);
            }
            return;
        }
        Cmd::Run { dry_run, limit, only_reencoded_originals } => CommandArgs { dry_run, limit, only_reencoded: only_reencoded_originals, ..blank("run") },
        Cmd::Consolidate { limit } => CommandArgs { limit, ..blank("consolidate") },
        Cmd::Scan => blank("scan"),
        Cmd::Adopt { path } => CommandArgs { path: Some(path), ..blank("adopt") },
        Cmd::Status => blank("status"),
        Cmd::Verify { deep } => CommandArgs { deep, ..blank("verify") },
        Cmd::RelocateOld => blank("relocate-old"),
        Cmd::ImportOld { path, limit } => CommandArgs { path: Some(path), limit, ..blank("import-old") },
        Cmd::CompareOld { path, out } => CommandArgs { path: Some(path), out, ..blank("compare-old") },
        Cmd::Sync => blank("sync"),
    };
    std::process::exit(kamroll_engine::run_command(args));
}
