use clap::{Parser, Subcommand};
use console::style;
use std::path::{Path, PathBuf};
use tracing::{error, info};

mod config;
mod logging;
mod organize;
mod replaygain;
mod scan;

use crate::config::load_or_create_config;
use crate::organize::organize_music_files;
use crate::replaygain::apply_replaygain;
use crate::scan::scan_for_music;

#[derive(Parser)]
#[command(name = "ufrume")]
#[command(about = "Multithreaded CLI tool to organize music files and compute ReplayGain tags")]
#[command(author = "PandaDEV, contact@pandadev.net")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(short, long, global = true)]
    threads: Option<usize>,

    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    Organize { dir: Option<PathBuf> },

    Replaygain { path: Option<PathBuf> },
}

fn resolve_path(arg: Option<PathBuf>) -> Result<PathBuf, String> {
    match arg {
        Some(path) => Ok(path),
        None => std::env::current_dir()
            .map_err(|e| format!("Failed to determine the current directory: {}", e)),
    }
}

fn configure_threads(threads: Option<usize>) {
    if let Some(count) = threads {
        if count == 0 {
            eprintln!("ERROR: Thread count must be greater than 0");
            std::process::exit(1);
        }

        if let Err(e) = rayon::ThreadPoolBuilder::new()
            .num_threads(count)
            .build_global()
        {
            error!("failed to configure thread pool: {}", e);
            eprintln!("ERROR: Failed to configure thread pool: {}", e);
            std::process::exit(1);
        }
        println!("  Threads: {}", style(count.to_string()).cyan());
    }
}

fn run_organize(dir: Option<PathBuf>, verbose: bool) {
    let dir = match resolve_path(dir) {
        Ok(dir) => dir,
        Err(e) => {
            error!("{}", e);
            eprintln!("ERROR: {}", e);
            std::process::exit(1);
        }
    };

    if !dir.is_dir() {
        error!("organize target is not a directory: {}", dir.display());
        eprintln!("ERROR: Not a directory: {}", dir.display());
        std::process::exit(1);
    }

    println!(
        "{} {}",
        style("[1/3]").bold().dim(),
        "Loading configuration..."
    );
    let config = match load_or_create_config() {
        Ok(config) => config,
        Err(e) => {
            error!("failed to load config: {}", e);
            eprintln!("ERROR: Failed to load config: {}", e);
            std::process::exit(1);
        }
    };

    info!("organizing directory in-place: {}", dir.display());
    println!("  Directory: {}", style(dir.display()).green());
    println!("  Mode:      {}", style("Move (in-place)").cyan());

    println!(
        "{} {}",
        style("[2/3]").bold().dim(),
        "Scanning music files..."
    );

    let music_files = match scan_for_music(&dir) {
        Ok(music_files) => {
            if music_files.is_empty() {
                println!("No music files found to organize.");
                return;
            }
            if verbose {
                print_scan_preview(&music_files);
            }
            music_files
        }
        Err(e) => {
            error!("failed to scan music files: {}", e);
            eprintln!("ERROR: Failed to scan music files: {}", e);
            std::process::exit(1);
        }
    };

    println!(
        "\n{} {}",
        style("[3/3]").bold().dim(),
        "Organizing music files..."
    );

    if let Err(e) = organize_music_files(&music_files, &dir, &config) {
        error!("failed to organize music files: {}", e);
        eprintln!("ERROR: Failed to organize music files: {}", e);
        std::process::exit(1);
    }
}

fn print_scan_preview(music_files: &[(PathBuf, scan::AudioMetadata)]) {
    println!("\nScan Results:");
    for (path, metadata) in music_files.iter().take(5) {
        println!(
            "  {} - {} - {}",
            metadata.artist.as_deref().unwrap_or("Unknown Artist"),
            metadata.title.as_deref().unwrap_or("Unknown Title"),
            style(path.file_name().unwrap_or_default().to_string_lossy()).dim()
        );
    }
    if music_files.len() > 5 {
        println!("  ... and {} more files", music_files.len() - 5);
    }
}

fn run_replaygain(path: Option<PathBuf>) {
    let path = match resolve_path(path) {
        Ok(path) => path,
        Err(e) => {
            error!("{}", e);
            eprintln!("ERROR: {}", e);
            std::process::exit(1);
        }
    };

    if !path.exists() {
        error!("replaygain target does not exist: {}", path.display());
        eprintln!("ERROR: Path does not exist: {}", path.display());
        std::process::exit(1);
    }

    info!("computing ReplayGain for: {}", path.display());
    println!(
        "{} {}",
        style("[1/1]").bold().dim(),
        "Computing ReplayGain..."
    );
    println!("  Target: {}", style(path.display()).green());

    if let Err(e) = apply_replaygain(&path) {
        error!("failed to compute ReplayGain: {}", e);
        eprintln!("ERROR: Failed to compute ReplayGain: {}", e);
        std::process::exit(1);
    }
}

fn main() {
    match logging::init() {
        Ok(log_path) => log_startup(&log_path),
        Err(e) => eprintln!("WARNING: Failed to initialise logging: {}", e),
    }

    let cli = Cli::parse();

    configure_threads(cli.threads);

    match cli.command {
        Commands::Organize { dir } => run_organize(dir, cli.verbose),
        Commands::Replaygain { path } => run_replaygain(path),
    }

    info!("done");
}

fn log_startup(log_path: &Path) {
    info!("ufrume v{} starting", env!("CARGO_PKG_VERSION"));
    info!("logging to {}", log_path.display());
}
