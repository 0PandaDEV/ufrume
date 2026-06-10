use indicatif::{ProgressBar, ProgressStyle};
use lofty::prelude::*;
use lofty::tag::ItemKey;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tracing::{debug, info, warn};
use walkdir::WalkDir;

pub const MUSIC_EXTENSIONS: &[&str] = &[
    "mp3", "flac", "m4a", "wav", "ogg", "aac", "opus", "aiff", "aif", "alac", "wma", "ape",
];

#[derive(Debug, Clone)]
pub struct AudioMetadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub track: Option<u16>,
    pub disc: Option<u16>,
}

pub fn is_music_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| MUSIC_EXTENSIONS.contains(&ext.to_lowercase().as_str()))
        .unwrap_or(false)
}

pub fn collect_music_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = WalkDir::new(dir)
        .follow_links(false)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.is_file() && is_music_file(path) {
                Some(path.to_path_buf())
            } else {
                None
            }
        })
        .collect();

    paths.sort_by(|a, b| {
        let mtime_a = std::fs::metadata(a)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let mtime_b = std::fs::metadata(b)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        mtime_b.cmp(&mtime_a)
    });

    paths
}

pub fn scan_for_music(
    input_dir: &Path,
) -> Result<Vec<(PathBuf, AudioMetadata)>, Box<dyn std::error::Error>> {
    let music_file_paths = collect_music_paths(input_dir);

    if music_file_paths.is_empty() {
        debug!("no music files found in {}", input_dir.display());
        return Ok(Vec::new());
    }

    let thread_count = rayon::current_num_threads();
    info!(
        "scanning {} files using {} threads",
        music_file_paths.len(),
        thread_count
    );
    println!(
        "  Processing {} files using {} threads",
        music_file_paths.len(),
        thread_count
    );

    let start_time = Instant::now();

    let pb = Arc::new(ProgressBar::new(music_file_paths.len() as u64));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let failed_extractions = Arc::new(Mutex::new(0));

    let results: Vec<Option<(PathBuf, AudioMetadata)>> = music_file_paths
        .par_iter()
        .map(|path| {
            if let Some(filename) = path.file_name() {
                pb.set_message(filename.to_string_lossy().to_string());
            }

            match extract_metadata(path) {
                Ok(metadata) => {
                    pb.inc(1);
                    Some((path.clone(), metadata))
                }
                Err(err) => {
                    warn!(
                        "failed to extract metadata from {}: {}",
                        path.display(),
                        err
                    );
                    eprintln!(
                        "  Failed to extract metadata from {}: {}",
                        path.display(),
                        err
                    );
                    *failed_extractions.lock().unwrap() += 1;
                    pb.inc(1);
                    None
                }
            }
        })
        .collect();

    pb.finish_and_clear();

    let duration = start_time.elapsed();

    let music_files: Vec<(PathBuf, AudioMetadata)> =
        results.into_iter().filter_map(|r| r).collect();

    let failed_count = *failed_extractions.lock().unwrap();
    if failed_count > 0 {
        info!(
            "{} files scanned, {} failed in {:.2}s",
            music_files.len(),
            failed_count,
            duration.as_secs_f64()
        );
        println!(
            "  {} files processed, {} failed in {:.2}s",
            music_files.len(),
            failed_count,
            duration.as_secs_f64()
        );
    } else {
        info!(
            "{} files scanned in {:.2}s",
            music_files.len(),
            duration.as_secs_f64()
        );
        println!(
            "  {} files processed in {:.2}s",
            music_files.len(),
            duration.as_secs_f64()
        );
    }

    Ok(music_files)
}

fn extract_metadata(path: &Path) -> Result<AudioMetadata, Box<dyn std::error::Error>> {
    let tagged_file = lofty::read_from_path(path)?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag());

    let Some(tag) = tag else {
        return Ok(AudioMetadata {
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            year: None,
            genre: None,
            track: None,
            disc: None,
        });
    };

    Ok(AudioMetadata {
        title: tag.title().map(|c| c.to_string()),
        artist: tag.artist().map(|c| c.to_string()),
        album: tag.album().map(|c| c.to_string()),
        album_artist: tag
            .get_string(ItemKey::AlbumArtist)
            .map(extract_first_artist),
        year: tag.date().map(|ts| ts.year as i32),
        genre: tag.genre().map(|c| c.to_string()),
        track: tag.track().map(|t| t as u16),
        disc: tag
            .get_string(ItemKey::DiscNumber)
            .and_then(|s| s.parse::<u16>().ok()),
    })
}

pub fn extract_first_artist(artist_string: &str) -> String {
    let delimiters = [
        ", ", " & ", " and ", " feat. ", " feat ", " ft. ", " ft ", " x ", " X ", " vs ", " vs. ",
        " with ", " + ", " / ",
    ];

    let mut earliest_pos = artist_string.len();

    for delimiter in &delimiters {
        if let Some(pos) = artist_string.find(delimiter) {
            if pos < earliest_pos {
                earliest_pos = pos;
            }
        }
    }

    if earliest_pos == artist_string.len() {
        artist_string.trim().to_string()
    } else {
        artist_string[..earliest_pos].trim().to_string()
    }
}
