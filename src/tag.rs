use crate::scan::{collect_music_paths, is_music_file};

use indicatif::{ProgressBar, ProgressStyle};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tracing::{debug, warn};

const MUSICBRAINZ_WS: &str = "https://musicbrainz.org/ws/2";
const USER_AGENT: &str = "ufrume/1.0 (https://github.com/0PandaDEV/ufrume)";

fn has_musicbrainz_tags(path: &Path) -> bool {
    let Ok(tagged_file) = lofty::read_from_path(path) else {
        return false;
    };
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return false;
    };
    tag.get_string(ItemKey::MusicBrainzRecordingId)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn read_existing_metadata(path: &Path) -> Option<ExistingMeta> {
    let tagged_file = lofty::read_from_path(path).ok()?;
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())?;

    let title = tag.title().map(|s| s.to_string());
    let artist = tag.artist().map(|s| s.to_string());
    let album = tag.album().map(|s| s.to_string());

    if title.is_none() && artist.is_none() {
        return None;
    }

    Some(ExistingMeta {
        title,
        artist,
        album,
    })
}

struct ExistingMeta {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct MbSearchResponse {
    recordings: Option<Vec<MbSearchRecording>>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct MbSearchRecording {
    id: String,
    title: Option<String>,
    score: f64,
    #[serde(rename = "first-release-date")]
    first_release_date: Option<String>,
    #[serde(rename = "artist-credit")]
    artist_credit: Option<Vec<MbSearchArtistCredit>>,
    releases: Option<Vec<MbSearchRelease>>,
}

#[derive(Debug, serde::Deserialize)]
struct MbSearchArtistCredit {
    name: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
#[allow(dead_code)]
struct MbSearchRelease {
    title: Option<String>,
    date: Option<String>,
    medium: Option<Vec<MbSearchMedium>>,
}

#[derive(Debug, serde::Deserialize)]
struct MbSearchMedium {
    track: Option<Vec<MbSearchTrack>>,
}

#[derive(Debug, serde::Deserialize)]
struct MbSearchTrack {
    number: Option<String>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
struct TagCandidate {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    year: Option<u32>,
    track: Option<u16>,
    genre: Option<String>,
    musicbrainz_recording_id: Option<String>,
    score: f64,
    source: String,
}

fn search_musicbrainz(
    query: &str,
    client: &reqwest::blocking::Client,
) -> Result<Vec<MbSearchRecording>, Box<dyn std::error::Error>> {
    let url = format!("{}/recording", MUSICBRAINZ_WS);
    let response = client
        .get(&url)
        .query(&[("query", query), ("fmt", "json"), ("limit", "10")])
        .send()?;

    if !response.status().is_success() {
        return Ok(Vec::new());
    }

    let body: MbSearchResponse = response.json()?;
    Ok(body.recordings.unwrap_or_default())
}

fn mb_recording_to_candidate(rec: &MbSearchRecording) -> TagCandidate {
    let artist = rec
        .artist_credit
        .as_ref()
        .and_then(|ac| ac.first())
        .and_then(|ac| ac.name.clone());

    let (album, year, track) = if let Some(releases) = &rec.releases {
        let first = releases.first();
        let album_title = first.and_then(|r| r.title.clone());
        let year = first
            .and_then(|r| r.date.as_ref())
            .and_then(|d| d.split('-').next())
            .and_then(|y| y.parse::<u32>().ok());
        let track = first
            .and_then(|r| r.medium.as_ref())
            .and_then(|m| m.first())
            .and_then(|m| m.track.as_ref())
            .and_then(|t| t.first())
            .and_then(|t| t.number.as_ref())
            .and_then(|n| n.parse::<u16>().ok());
        (album_title, year, track)
    } else {
        let year = rec
            .first_release_date
            .as_ref()
            .and_then(|d| d.split('-').next())
            .and_then(|y| y.parse::<u32>().ok());
        (None, year, None)
    };

    TagCandidate {
        title: rec.title.clone(),
        artist,
        album,
        album_artist: None,
        year,
        track,
        genre: None,
        musicbrainz_recording_id: Some(rec.id.clone()),
        score: rec.score,
        source: "MusicBrainz".to_string(),
    }
}

fn candidate_match_score(
    candidate: &TagCandidate,
    existing_artist: Option<&str>,
    existing_title: Option<&str>,
) -> f64 {
    let mut score = 0.0;

    if let Some(ref ea) = existing_artist {
        if let Some(ref ca) = candidate.artist {
            if ca.to_lowercase() == ea.to_lowercase() {
                score += 100.0;
            } else if ca.to_lowercase().contains(&ea.to_lowercase())
                || ea.to_lowercase().contains(&ca.to_lowercase())
            {
                score += 50.0;
            }
        }
    }

    if let Some(ref et) = existing_title {
        if let Some(ref ct) = candidate.title {
            if ct.to_lowercase() == et.to_lowercase() {
                score += 100.0;
            } else if ct.to_lowercase().contains(&et.to_lowercase())
                || et.to_lowercase().contains(&ct.to_lowercase())
            {
                score += 50.0;
            }
        }
    }

    if let Some(ref ca) = candidate.album {
        if let Some(ref ea) = existing_artist {
            if ca.to_lowercase().contains(&ea.to_lowercase()) {
                score += 10.0;
            }
        }
    }

    score
}

fn select_candidate(
    candidates: &[TagCandidate],
    rel_path: &str,
    existing: Option<&ExistingMeta>,
) -> Option<TagCandidate> {
    if candidates.is_empty() {
        return None;
    }

    if candidates.len() == 1 && candidates[0].score >= 0.8 {
        return Some(candidates[0].clone());
    }

    println!();
    println!(
        "  {} {}",
        console::style("File:").bold(),
        console::style(rel_path).cyan()
    );

    if let Some(meta) = existing {
        println!(
            "  {} {}",
            console::style("Current tags:").bold(),
            [
                meta.artist.as_deref().map(|v| format!("artist={}", v)),
                meta.title.as_deref().map(|v| format!("title={}", v)),
                meta.album.as_deref().map(|v| format!("album={}", v)),
            ]
            .iter()
            .filter_map(|x| x.clone())
            .collect::<Vec<String>>()
            .join(", ")
        );
    } else {
        println!("  {}", console::style("Current tags: (none)").bold());
    }

    println!("  {}", console::style("0: Skip").dim());

    for (i, c) in candidates.iter().enumerate() {
        let artist = c.artist.as_deref().unwrap_or("Unknown");
        let title = c.title.as_deref().unwrap_or("Unknown");
        let album = c.album.as_deref().unwrap_or("?");
        let year = c
            .year
            .map(|y| y.to_string())
            .unwrap_or_else(|| "?".to_string());
        let track = c
            .track
            .map(|t| t.to_string())
            .unwrap_or_else(|| "?".to_string());

        println!(
            "  {}: {} - {} [{}] ({}) track {}",
            console::style(i + 1).green().bold(),
            console::style(artist).yellow(),
            console::style(title).yellow(),
            console::style(album).dim(),
            console::style(year).dim(),
            console::style(track).dim(),
        );
    }

    loop {
        print!("\n  {}: ", console::style("Enter number").bold());
        io::stdout().flush().ok();

        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => return None,
            Ok(_) => {}
            Err(_) => return None,
        }

        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        let choice: usize = match input.parse() {
            Ok(n) => n,
            Err(_) => {
                println!("  {}", console::style("Invalid number").red());
                continue;
            }
        };

        if choice == 0 {
            return None;
        }
        if choice <= candidates.len() {
            return Some(candidates[choice - 1].clone());
        }
        println!("  {}", console::style("Invalid number").red());
    }
}

fn write_metadata_tags(
    path: &Path,
    candidate: &TagCandidate,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut tagged_file = lofty::read_from_path(path)?;

    if tagged_file.primary_tag().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged_file
        .primary_tag_mut()
        .expect("a primary tag was just ensured");

    if let Some(ref title) = candidate.title {
        tag.insert_text(ItemKey::TrackTitle, title.clone());
    }
    if let Some(ref artist) = candidate.artist {
        tag.insert_text(ItemKey::TrackArtist, artist.clone());
    }
    if let Some(ref album) = candidate.album {
        tag.insert_text(ItemKey::AlbumTitle, album.clone());
    }
    if let Some(ref album_artist) = candidate.album_artist {
        tag.insert_text(ItemKey::AlbumArtist, album_artist.clone());
    }
    if let Some(year) = candidate.year {
        tag.insert_text(ItemKey::RecordingDate, year.to_string());
    }
    if let Some(track) = candidate.track {
        tag.insert_text(ItemKey::TrackNumber, track.to_string());
    }
    if let Some(ref mbid) = candidate.musicbrainz_recording_id {
        tag.insert_text(ItemKey::MusicBrainzRecordingId, mbid.clone());
    }

    tag.save_to_path(path, WriteOptions::default())?;

    Ok(())
}

pub fn tag_music_files(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let files: Vec<PathBuf> = if path.is_file() {
        if !is_music_file(path) {
            return Err(format!("{} is not a supported audio file", path.display()).into());
        }
        vec![path.to_path_buf()]
    } else {
        collect_music_paths(path)
    };

    if files.is_empty() {
        println!("No music files found to tag.");
        return Ok(());
    }

    let mut sorted_files = files;
    sorted_files.sort_by(|a, b| {
        let mtime_a = std::fs::metadata(a)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mtime_b = std::fs::metadata(b)
            .and_then(|m| m.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        mtime_b.cmp(&mtime_a)
    });

    let client = reqwest::blocking::Client::builder()
        .user_agent(USER_AGENT)
        .build()?;

    let pb = ProgressBar::new(sorted_files.len() as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let mut tagged_count = 0usize;
    let mut skipped_count = 0usize;
    let mut failed_count = 0usize;

    for file in &sorted_files {
        let rel_path = file
            .strip_prefix(path)
            .unwrap_or(file.as_path())
            .to_string_lossy()
            .to_string();

        pb.set_message(rel_path.clone());

        if has_musicbrainz_tags(file) {
            skipped_count += 1;
            pb.inc(1);
            continue;
        }

        match process_file(file, &rel_path, &client) {
            Ok(true) => {
                tagged_count += 1;
            }
            Ok(false) => {
                skipped_count += 1;
            }
            Err(e) => {
                warn!("failed to tag {}: {}", file.display(), e);
                eprintln!("  Failed to tag {}: {}", file.display(), e);
                failed_count += 1;
            }
        }

        pb.inc(1);
    }

    pb.finish_and_clear();

    println!(
        "  {} files tagged, {} skipped, {} failed",
        console::style(tagged_count).green(),
        console::style(skipped_count).dim(),
        console::style(failed_count).red()
    );

    Ok(())
}

fn process_file(
    path: &Path,
    rel_path: &str,
    client: &reqwest::blocking::Client,
) -> Result<bool, Box<dyn std::error::Error>> {
    let existing = read_existing_metadata(path);

    let mut candidates: Vec<TagCandidate> = Vec::new();

    if let Some(ref meta) = existing {
        if let Some(ref title) = meta.title {
            let query = match (&meta.artist, &meta.album) {
                (Some(artist), Some(album)) => {
                    format!(
                        "recording:\"{}\" AND artist:\"{}\" AND release:\"{}\"",
                        title, artist, album
                    )
                }
                (Some(artist), None) => {
                    format!("recording:\"{}\" AND artist:\"{}\"", title, artist)
                }
                (None, Some(album)) => {
                    format!("recording:\"{}\" AND release:\"{}\"", title, album)
                }
                (None, None) => {
                    format!("recording:\"{}\"", title)
                }
            };

            let results = search_musicbrainz(&query, client)?;
            for rec in &results {
                candidates.push(mb_recording_to_candidate(rec));
            }

            if candidates.is_empty() {
                if let Some(ref artist) = meta.artist {
                    let query = format!("recording:\"{}\" AND artist:\"{}\"", title, artist);
                    let results = search_musicbrainz(&query, client)?;
                    for rec in &results {
                        candidates.push(mb_recording_to_candidate(rec));
                    }
                }
            }

            if candidates.is_empty() {
                let query = format!("recording:\"{}\"", title);
                let results = search_musicbrainz(&query, client)?;
                for rec in &results {
                    candidates.push(mb_recording_to_candidate(rec));
                }
            }
        }
    }

    if candidates.is_empty() {
        debug!("no candidates for {}", path.display());
        return Ok(false);
    }

    candidates.sort_by(|a, b| {
        let existing_artist = existing
            .as_ref()
            .and_then(|e| e.artist.as_deref())
            .map(|s| s.to_lowercase());
        let existing_title = existing
            .as_ref()
            .and_then(|e| e.title.as_deref())
            .map(|s| s.to_lowercase());

        let score_a =
            candidate_match_score(a, existing_artist.as_deref(), existing_title.as_deref());
        let score_b =
            candidate_match_score(b, existing_artist.as_deref(), existing_title.as_deref());
        score_b
            .partial_cmp(&score_a)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates.truncate(10);

    let chosen = match select_candidate(&candidates, rel_path, existing.as_ref()) {
        Some(c) => c,
        None => return Ok(false),
    };

    write_metadata_tags(path, &chosen)?;
    Ok(true)
}
