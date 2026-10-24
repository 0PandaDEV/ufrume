use crate::scan::{collect_music_paths, is_music_file};

use console::{Term, truncate_str};
use indicatif::{ProgressBar, ProgressStyle};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};

const OMM_API: &str = "https://api.vleer.app/metadata/v1";
const MAX_ATTEMPTS: u32 = 3;
const PROGRESS_PREFIX_WIDTH: usize = 75;
const MAX_PARAM_CHARS: usize = 256;

struct ExistingMeta {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration_ms: Option<u128>,
}

#[derive(Debug, Deserialize)]
struct OmmResponse {
    data: OmmSong,
}

#[derive(Debug, Deserialize)]
struct OmmSong {
    attributes: OmmSongAttributes,
    relationships: Option<OmmSongRelationships>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmmSongAttributes {
    name: String,
    album_name: Option<String>,
    #[serde(default)]
    artist_names: Vec<String>,
    isrc: Option<String>,
    track_number: Option<u32>,
    disc_number: Option<u32>,
    genres: Option<Vec<String>>,
    release_date: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OmmSongRelationships {
    albums: Option<OmmAlbumList>,
}

#[derive(Debug, Deserialize)]
struct OmmAlbumList {
    data: Vec<OmmAlbum>,
}

#[derive(Debug, Deserialize)]
struct OmmAlbum {
    attributes: OmmAlbumAttributes,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OmmAlbumAttributes {
    name: String,
    #[serde(default)]
    artist_names: Vec<String>,
    release_date: Option<String>,
}

#[derive(Debug, Clone)]
struct TagCandidate {
    title: String,
    artists: Vec<String>,
    album: Option<String>,
    album_artists: Vec<String>,
    date: Option<String>,
    track: Option<u32>,
    disc: Option<u32>,
    genre: Option<String>,
    isrc: Option<String>,
}

fn has_omm_tags(path: &Path) -> bool {
    let Ok(tagged_file) = lofty::read_from_path(path) else {
        return false;
    };
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return false;
    };
    tag.get_string(ItemKey::Isrc)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

fn read_existing_metadata(path: &Path) -> Option<ExistingMeta> {
    let tagged_file = lofty::read_from_path(path).ok()?;
    let duration_ms = Some(tagged_file.properties().duration().as_millis()).filter(|d| *d > 0);
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
        duration_ms,
    })
}

fn truncate_param(value: &str) -> String {
    value.chars().take(MAX_PARAM_CHARS).collect()
}

fn song_to_candidate(song: OmmSong) -> TagCandidate {
    let attrs = song.attributes;
    let album = song
        .relationships
        .and_then(|r| r.albums)
        .and_then(|a| a.data.into_iter().next())
        .map(|a| a.attributes);

    let (album_name, album_artists, album_date) = match album {
        Some(a) => (Some(a.name), a.artist_names, a.release_date),
        None => (None, Vec::new(), None),
    };

    TagCandidate {
        title: attrs.name,
        artists: attrs.artist_names,
        album: attrs.album_name.or(album_name),
        album_artists,
        date: attrs.release_date.or(album_date),
        track: attrs.track_number,
        disc: attrs.disc_number,
        genre: attrs.genres.filter(|g| !g.is_empty()).map(|g| g.join("; ")),
        isrc: attrs.isrc,
    }
}

fn identify_song(
    meta: &ExistingMeta,
    title: &str,
    client: &reqwest::blocking::Client,
) -> Result<Option<TagCandidate>, Box<dyn std::error::Error>> {
    let mut query: Vec<(&str, String)> = vec![("name", truncate_param(title))];
    if let Some(ref artist) = meta.artist {
        query.push(("artist", truncate_param(artist)));
    }
    if let Some(ref album) = meta.album {
        query.push(("album", truncate_param(album)));
    }
    if let Some(duration) = meta.duration_ms {
        query.push(("duration", duration.to_string()));
    }

    let url = format!("{}/identify/song", OMM_API);

    let mut last_status = None;

    for attempt in 1..=MAX_ATTEMPTS {
        let response = client.get(&url).query(&query).send()?;
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            last_status = Some(status);
            if attempt == MAX_ATTEMPTS {
                break;
            }
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(u64::from(attempt));
            sleep(Duration::from_secs(wait.clamp(1, 30)));
            continue;
        }

        if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::BAD_REQUEST {
            return Ok(None);
        }

        if !status.is_success() {
            return Err(format!("API returned {}", status.as_u16()).into());
        }

        let body: OmmResponse = response.json()?;
        return Ok(Some(song_to_candidate(body.data)));
    }

    let code = last_status.map(|s| s.as_u16()).unwrap_or_default();
    Err(format!("API unavailable ({})", code).into())
}

fn fit(text: &str, reserved: usize) -> String {
    let width = Term::stdout().size().1 as usize;
    truncate_str(text, width.saturating_sub(reserved).max(20), "…").into_owned()
}

fn names_match(a: &str, b: &str) -> bool {
    let a = a.trim().to_lowercase();
    let b = b.trim().to_lowercase();
    !a.is_empty() && !b.is_empty() && (a == b || a.contains(&b) || b.contains(&a))
}

fn prioritize_artist(names: &mut [String], preferred: &str) {
    if let Some(pos) = names
        .iter()
        .position(|n| names_match(n, preferred) || names_match(preferred, n))
    {
        names[..=pos].rotate_right(1);
    }
}

fn candidate_matches(candidate: &TagCandidate, existing: &ExistingMeta, title: &str) -> bool {
    if !names_match(&candidate.title, title) {
        return false;
    }

    match existing.artist.as_deref() {
        Some(artist) if !candidate.artists.is_empty() => candidate
            .artists
            .iter()
            .any(|name| names_match(name, artist) || names_match(artist, name)),
        _ => true,
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

    tag.insert_text(ItemKey::TrackTitle, candidate.title.clone());
    if !candidate.artists.is_empty() {
        tag.insert_text(ItemKey::TrackArtist, candidate.artists.join(", "));
    }
    if let Some(ref album) = candidate.album {
        tag.insert_text(ItemKey::AlbumTitle, album.clone());
    }
    if !candidate.album_artists.is_empty() {
        tag.insert_text(ItemKey::AlbumArtist, candidate.album_artists.join(", "));
    }
    if let Some(ref date) = candidate.date {
        tag.insert_text(ItemKey::RecordingDate, date.clone());
    }
    if let Some(track) = candidate.track {
        tag.insert_text(ItemKey::TrackNumber, track.to_string());
    }
    if let Some(disc) = candidate.disc {
        tag.insert_text(ItemKey::DiscNumber, disc.to_string());
    }
    if let Some(ref genre) = candidate.genre {
        tag.insert_text(ItemKey::Genre, genre.clone());
    }
    if let Some(ref isrc) = candidate.isrc {
        tag.insert_text(ItemKey::Isrc, isrc.clone());
    }

    tag.save_to_path(path, WriteOptions::default())?;

    Ok(())
}

pub fn tag_music_files(path: &Path, force: bool) -> Result<(), Box<dyn std::error::Error>> {
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
        .user_agent(format!(
            "ufrume/{} (https://github.com/0PandaDEV/ufrume)",
            env!("CARGO_PKG_VERSION")
        ))
        .timeout(Duration::from_secs(30))
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

        pb.set_message(fit(&rel_path, PROGRESS_PREFIX_WIDTH));

        if !force && has_omm_tags(file) {
            skipped_count += 1;
            pb.inc(1);
            continue;
        }

        match process_file(file, &client) {
            Ok(true) => {
                tagged_count += 1;
            }
            Ok(false) => {
                skipped_count += 1;
            }
            Err(e) => {
                warn!("failed to tag {}: {}", file.display(), e);
                let name = file.file_name().unwrap_or_default().to_string_lossy();
                pb.println(fit(&format!("  Failed: {} - {}", name, e), 0));
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
    client: &reqwest::blocking::Client,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(existing) = read_existing_metadata(path) else {
        debug!("no usable tags in {}", path.display());
        return Ok(false);
    };
    let Some(title) = existing.title.clone().filter(|t| !t.trim().is_empty()) else {
        debug!("no title in {}", path.display());
        return Ok(false);
    };

    let Some(candidate) = identify_song(&existing, &title, client)? else {
        debug!("no match for {}", path.display());
        return Ok(false);
    };

    if !candidate_matches(&candidate, &existing, &title) {
        debug!(
            "rejected mismatched match '{}' for {}",
            candidate.title,
            path.display()
        );
        return Ok(false);
    }

    let mut candidate = candidate;
    if let Some(ref artist) = existing.artist {
        prioritize_artist(&mut candidate.artists, artist);
        prioritize_artist(&mut candidate.album_artists, artist);
    }

    write_metadata_tags(path, &candidate)?;
    Ok(true)
}
