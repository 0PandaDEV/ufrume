use std::fs::{self, File};
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

fn log_file_path() -> PathBuf {
    let base = dirs::data_local_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("ufrume").join("ufrume.log")
}

pub fn init() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let path = log_file_path();

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = File::create(&path)?;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn,ufrume=debug"));

    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_env_filter(filter)
        .with_writer(move || file.try_clone().expect("failed to clone log file handle"))
        .init();

    Ok(path)
}
