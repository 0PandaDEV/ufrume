use crate::{config::Config, scan::AudioMetadata};

use indicatif::{ProgressBar, ProgressStyle};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};
use rayon::prelude::*;
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};
use tracing::{debug, info, warn};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct MetadataKey {
    artist: String,
    album: String,
    title: String,
    track: Option<u16>,
    disc: Option<u16>,
}

#[derive(Debug)]
pub struct OrganizeResult {
    pub moved: usize,
    pub skipped: usize,
    pub failed: usize,
    pub duplicates: usize,
}

pub fn organize_music_files(
    music_files: &[(PathBuf, AudioMetadata)],
    output_dir: &Path,
    config: &Config,
    normalize: bool,
) -> Result<OrganizeResult, Box<dyn std::error::Error>> {
    if music_files.is_empty() {
        return Ok(OrganizeResult {
            moved: 0,
            skipped: 0,
            failed: 0,
            duplicates: 0,
        });
    }

    let mut sorted_files: Vec<(PathBuf, AudioMetadata)> = music_files.to_vec();
    sorted_files.sort_by(|a, b| {
        let mtime_a = std::fs::metadata(&a.0)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let mtime_b = std::fs::metadata(&b.0)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        mtime_b.cmp(&mtime_a)
    });

    let start_time = Instant::now();

    let pb = Arc::new(ProgressBar::new(sorted_files.len() as u64));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let failed = Arc::new(Mutex::new(0usize));

    let multi_disc_albums: HashSet<(String, String)> = {
        let mut disc_counts: HashMap<(String, String), HashSet<u16>> = HashMap::new();
        for (_, meta) in &sorted_files {
            if let (Some(disc), Some(album)) = (meta.disc, &meta.album) {
                let artist = meta
                    .album_artist
                    .as_ref()
                    .or(meta.artist.as_ref())
                    .cloned()
                    .unwrap_or_default();
                disc_counts
                    .entry((artist, album.clone()))
                    .or_default()
                    .insert(disc);
            }
        }
        disc_counts
            .into_iter()
            .filter(|(_, discs)| discs.len() > 1)
            .map(|(k, _)| k)
            .collect()
    };
    let multi_disc_albums = Arc::new(multi_disc_albums);

    let planned: Vec<Option<Planned>> = sorted_files
        .par_iter()
        .map(|(source_path, metadata)| {
            if let Some(filename) = source_path.file_name() {
                pb.set_message(filename.to_string_lossy().to_string());
            }

            let plan = plan_file(
                source_path,
                metadata,
                output_dir,
                config,
                normalize,
                &multi_disc_albums,
            );
            pb.inc(1);

            match plan {
                Ok(plan) => plan,
                Err(e) => {
                    warn!("failed to organize {}: {}", source_path.display(), e);
                    *failed.lock().unwrap() += 1;
                    None
                }
            }
        })
        .collect();

    let missing = planned.iter().filter(|p| p.is_none()).count();
    let planned: Vec<Planned> = planned.into_iter().flatten().collect();

    let Resolution {
        moves,
        stayed,
        duplicates,
        removals,
    } = resolve_plan(&planned, config);

    for path in removals {
        if let Err(e) = fs::remove_file(&path) {
            warn!("failed to remove {}: {}", path.display(), e);
        }
    }

    let (moved, blocked, move_failures) = execute_moves(moves, &config.rules.handle_duplicates);
    *failed.lock().unwrap() += move_failures;

    pb.finish_and_clear();

    cleanup_empty_dirs(output_dir);

    let duration = start_time.elapsed();
    let result = OrganizeResult {
        moved,
        skipped: stayed + missing,
        failed: *failed.lock().unwrap(),
        duplicates: duplicates + blocked,
    };

    info!(
        "organized {} files in {:.2}s (skipped {}, duplicates {}, failed {})",
        result.moved,
        duration.as_secs_f64(),
        result.skipped,
        result.duplicates,
        result.failed
    );

    println!(
        "  {} files moved in {:.2}s",
        result.moved,
        duration.as_secs_f64()
    );
    if result.skipped > 0 {
        println!("  {} files skipped", result.skipped);
    }
    if result.duplicates > 0 {
        println!("  {} duplicates handled", result.duplicates);
    }
    if result.failed > 0 {
        println!("  {} files failed", result.failed);
    }

    Ok(result)
}

struct Planned {
    source: PathBuf,
    target: PathBuf,
    key: MetadataKey,
    fallback: bool,
}

struct Resolution {
    moves: Vec<(PathBuf, PathBuf)>,
    stayed: usize,
    duplicates: usize,
    removals: Vec<PathBuf>,
}

fn plan_file(
    source_path: &Path,
    metadata: &AudioMetadata,
    output_dir: &Path,
    config: &Config,
    normalize: bool,
    multi_disc_albums: &Arc<HashSet<(String, String)>>,
) -> Result<Option<Planned>, Box<dyn std::error::Error>> {
    let normalized_storage;
    let metadata: &AudioMetadata = if normalize {
        normalized_storage = normalize_metadata(metadata, config);
        if let Err(e) = write_normalized_tags(source_path, metadata, &normalized_storage) {
            warn!(
                "failed to write normalized tags for {}: {}",
                source_path.display(),
                e
            );
        }
        &normalized_storage
    } else {
        metadata
    };

    let relative_path = match generate_target_path(source_path, metadata, config, multi_disc_albums)
    {
        Some(path) => path,
        None => {
            if config.rules.handle_missing_metadata == "skip" {
                return Ok(None);
            } else {
                generate_fallback_path(source_path, config)
            }
        }
    };

    let target = if source_path.starts_with(output_dir) {
        let dir_name = output_dir.file_name().unwrap_or_default().to_string_lossy();
        let template_str = relative_path.to_string_lossy();
        let stripped = if template_str.starts_with(&*format!("{}/", dir_name)) {
            PathBuf::from(&template_str[dir_name.len() + 1..])
        } else {
            relative_path.clone()
        };
        output_dir.join(&stripped)
    } else {
        output_dir.join(&relative_path)
    };

    let (key, fallback) = match create_metadata_key(metadata) {
        Some(key) => (key, false),
        None => (
            MetadataKey {
                artist: metadata
                    .album_artist
                    .clone()
                    .or_else(|| metadata.artist.clone())
                    .unwrap_or_else(|| "Unknown".to_string()),
                album: metadata
                    .album
                    .clone()
                    .unwrap_or_else(|| "Unknown".to_string()),
                title: source_path
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string(),
                track: None,
                disc: None,
            },
            true,
        ),
    };

    Ok(Some(Planned {
        source: source_path.to_path_buf(),
        target,
        key,
        fallback,
    }))
}

fn resolve_plan(planned: &[Planned], config: &Config) -> Resolution {
    let policy = config.rules.handle_duplicates.as_str();

    let mut order: Vec<usize> = (0..planned.len()).collect();
    order.sort_by_key(|&i| planned[i].source != planned[i].target);

    let index_of: HashMap<&Path, usize> = planned
        .iter()
        .enumerate()
        .map(|(i, p)| (p.source.as_path(), i))
        .collect();

    let mut used_keys: HashMap<MetadataKey, PathBuf> = HashMap::new();
    let mut claimed: HashMap<PathBuf, usize> = HashMap::new();
    let mut cancelled: HashSet<usize> = HashSet::new();
    let mut moves: Vec<(usize, PathBuf)> = Vec::new();
    let mut stayed = 0;
    let mut duplicates = 0;
    let mut removals = Vec::new();

    for i in order {
        let plan = &planned[i];
        let mut target = plan.target.clone();

        if used_keys.contains_key(&plan.key) {
            match policy {
                "skip" => {
                    duplicates += 1;
                    continue;
                }
                "rename" => {
                    target = handle_duplicate_rename(&target, &plan.key, &mut used_keys);
                }
                "overwrite" if !plan.fallback => {
                    if let Some(old) = used_keys.get(&plan.key)
                        && let Some(&old_index) = index_of.get(old.as_path())
                    {
                        cancelled.insert(old_index);
                        removals.push(old.clone());
                    }
                    used_keys.insert(plan.key.clone(), plan.source.clone());
                }
                _ => {}
            }
        } else {
            used_keys.insert(plan.key.clone(), plan.source.clone());
        }

        if target == plan.source {
            claimed.insert(target, i);
            stayed += 1;
            continue;
        }

        let occupied_elsewhere = claimed.contains_key(&target)
            || (target.exists() && !index_of.contains_key(target.as_path()));
        if occupied_elsewhere {
            match policy {
                "overwrite" => {}
                "rename" => {
                    target = first_free_path_excluding(&target, &claimed);
                }
                _ => {
                    duplicates += 1;
                    continue;
                }
            }
        }

        claimed.insert(target.clone(), i);
        moves.push((i, target));
    }

    let moves = moves
        .into_iter()
        .filter(|(i, _)| !cancelled.contains(i))
        .map(|(i, target)| (planned[i].source.clone(), target))
        .collect();

    Resolution {
        moves,
        stayed,
        duplicates,
        removals,
    }
}

fn execute_moves(mut pending: Vec<(PathBuf, PathBuf)>, policy: &str) -> (usize, usize, usize) {
    let mut moved = 0;
    let mut failures = 0;

    loop {
        let mut progressed = false;
        let mut remaining = Vec::new();

        for (source, target) in pending {
            if target.exists() && policy != "overwrite" {
                remaining.push((source, target));
                continue;
            }
            match move_file(&source, &target) {
                Ok(()) => {
                    debug!("moved {} -> {}", source.display(), target.display());
                    moved += 1;
                    progressed = true;
                }
                Err(e) => {
                    warn!("failed to move {}: {}", source.display(), e);
                    failures += 1;
                }
            }
        }

        pending = remaining;
        if pending.is_empty() {
            break;
        }
        if progressed {
            continue;
        }

        let blocker = pending.iter().enumerate().find_map(|(i, (_, target))| {
            pending
                .iter()
                .position(|(source, _)| source == target)
                .filter(|&j| j != i)
        });
        let Some(j) = blocker else {
            break;
        };

        let (source, _) = &pending[j];
        let parked = source.with_file_name(format!(".ufrume-moving-{}-{}", std::process::id(), j));
        match fs::rename(source, &parked) {
            Ok(()) => pending[j].0 = parked,
            Err(e) => {
                warn!("failed to park {}: {}", source.display(), e);
                failures += 1;
                pending.remove(j);
            }
        }
    }

    (moved, pending.len(), failures)
}

fn first_free_path_excluding(target_path: &Path, claimed: &HashMap<PathBuf, usize>) -> PathBuf {
    let stem = target_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = target_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = target_path.parent().unwrap_or(Path::new(""));

    (1..)
        .map(|counter| parent.join(format!("{} ({}){}", stem, counter, extension)))
        .find(|candidate| !candidate.exists() && !claimed.contains_key(candidate))
        .expect("an unused file name exists")
}

fn move_file(source_path: &Path, target_path: &Path) -> io::Result<()> {
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    match fs::rename(source_path, target_path) {
        Ok(()) => Ok(()),
        Err(_) => {
            fs::copy(source_path, target_path)?;
            fs::remove_file(source_path)?;
            Ok(())
        }
    }
}

fn generate_target_path(
    source_path: &Path,
    metadata: &AudioMetadata,
    config: &Config,
    multi_disc_albums: &HashSet<(String, String)>,
) -> Option<PathBuf> {
    let structure = if is_compilation(metadata) {
        config
            .organization
            .compilation_structure
            .as_ref()
            .unwrap_or(&config.organization.structure)
    } else {
        &config.organization.structure
    };

    let path_str =
        match replace_placeholders(structure, source_path, metadata, config, multi_disc_albums) {
            Some(path) => path,
            None => {
                let pruned = prune_template(structure, metadata)?;
                replace_placeholders(&pruned, source_path, metadata, config, multi_disc_albums)?
            }
        };
    let sanitized_path = sanitize_path(&path_str, config);
    Some(PathBuf::from(sanitized_path))
}

fn placeholder_available(name: &str, metadata: &AudioMetadata) -> bool {
    match name {
        "artist" => metadata.artist.is_some() || metadata.album_artist.is_some(),
        "title" => metadata.title.is_some(),
        "album" => metadata.album.is_some(),
        "year" => metadata.year.is_some(),
        "track" => metadata.track.is_some(),
        "disc" => metadata.disc.is_some(),
        "genre" => metadata.genre.is_some(),
        _ => true,
    }
}

fn part_placeholders(part: &str) -> Vec<&str> {
    let mut names = Vec::new();
    let mut rest = part;
    while let Some(start) = rest.find('{') {
        let Some(len) = rest[start..].find('}') else {
            break;
        };
        let inner = &rest[start + 1..start + len];
        names.push(inner.split(':').next().unwrap_or(inner));
        rest = &rest[start + len + 1..];
    }
    names
}

fn prune_template(template: &str, metadata: &AudioMetadata) -> Option<String> {
    let mut used_metadata = false;
    let mut segments: Vec<String> = Vec::new();

    let all: Vec<&str> = template.split('/').collect();
    let last_index = all.len() - 1;
    let mut file_segment = "{filename}".to_string();

    for (index, segment) in all.iter().enumerate() {
        let kept: Vec<&str> = segment
            .split(" - ")
            .filter(|part| {
                part_placeholders(part)
                    .iter()
                    .all(|name| placeholder_available(name, metadata))
            })
            .collect();
        let joined = kept.join(" - ");
        if joined.is_empty() {
            continue;
        }
        let names = part_placeholders(&joined);
        if index == last_index {
            if names.contains(&"title") {
                used_metadata = true;
                file_segment = joined;
            } else if names.contains(&"filename") {
                file_segment = joined;
            }
        } else {
            if names.iter().any(|name| *name != "filename") {
                used_metadata = true;
            }
            segments.push(joined);
        }
    }

    if !used_metadata {
        return None;
    }

    segments.push(file_segment);
    Some(segments.join("/"))
}

fn generate_fallback_path(source_path: &Path, config: &Config) -> PathBuf {
    let filename = source_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy();

    let fallback_str = config
        .organization
        .fallback_structure
        .replace("{filename}", &filename);

    let sanitized_path = sanitize_path(&fallback_str, config);
    PathBuf::from(sanitized_path)
}

fn replace_placeholders(
    template: &str,
    source_path: &Path,
    metadata: &AudioMetadata,
    config: &Config,
    multi_disc_albums: &HashSet<(String, String)>,
) -> Option<String> {
    let mut result = template.to_string();

    if template.contains("{artist}") {
        if is_compilation(metadata) {
            if let Some(artist) = &metadata.artist {
                result = result.replace("{artist}", &sanitize_metadata_value(artist, config));
            } else {
                return None;
            }
        } else if let Some(album_artist) = &metadata.album_artist {
            result = result.replace(
                "{artist}",
                &sanitize_metadata_value(primary_artist(album_artist), config),
            );
        } else if let Some(artist) = &metadata.artist {
            result = result.replace(
                "{artist}",
                &sanitize_metadata_value(primary_artist(artist), config),
            );
        } else {
            return None;
        }
    }

    if let Some(title) = &metadata.title {
        result = result.replace("{title}", &sanitize_metadata_value(title, config));
    } else if template.contains("{title}") {
        return None;
    }

    if let Some(album) = &metadata.album {
        result = result.replace("{album}", &sanitize_metadata_value(album, config));
    } else if template.contains("{album}") {
        return None;
    }

    if let Some(year) = metadata.year {
        result = result.replace("{year}", &year.to_string());
    } else if template.contains("{year}") {
        return None;
    }

    if template.contains("{track") {
        if let Some(track) = metadata.track {
            if let Some(start) = template.find("{track")
                && let Some(end) = template[start..].find('}')
            {
                let full_placeholder = &template[start..start + end + 1];
                if full_placeholder.contains(':') {
                    let format_part = &full_placeholder[7..full_placeholder.len() - 1];
                    if format_part == "02" {
                        result = result.replace(full_placeholder, &format!("{:02}", track));
                    } else {
                        result = result.replace(full_placeholder, &track.to_string());
                    }
                } else {
                    result = result.replace("{track}", &track.to_string());
                }
            }
        } else if template.contains("{track") {
            return None;
        }
    }

    let is_multi_disc = metadata.disc.is_some_and(|_| {
        let artist = metadata
            .album_artist
            .as_ref()
            .or(metadata.artist.as_ref())
            .cloned()
            .unwrap_or_default();
        let album = metadata.album.as_ref().cloned().unwrap_or_default();
        multi_disc_albums.contains(&(artist, album))
    });

    if template.contains("{disc") {
        if let Some(disc) = metadata.disc {
            if is_multi_disc {
                if let Some(start) = template.find("{disc")
                    && let Some(end) = template[start..].find('}')
                {
                    let full_placeholder = &template[start..start + end + 1];
                    if full_placeholder.contains(':') {
                        let format_part = &full_placeholder[7..full_placeholder.len() - 1];
                        if format_part == "02" {
                            result = result.replace(full_placeholder, &format!("{:02}", disc));
                        } else {
                            result = result.replace(full_placeholder, &disc.to_string());
                        }
                    } else {
                        result = result.replace("{disc}", &format!("Disc {}", disc));
                    }
                }
            } else {
                if let Some(start) = template.find("{disc")
                    && let Some(end) = template[start..].find('}')
                {
                    let full_placeholder = &template[start..start + end + 1];
                    result = result.replace(&format!("/{}/", full_placeholder), "/");
                    result = result.replace(&format!("/{}", full_placeholder), "");
                    result = result.replace(&format!("{}/", full_placeholder), "");
                    result = result.replace(full_placeholder, "");
                }
                let _ = disc;
            }
        } else if template.contains("{disc") {
            return None;
        }
    } else if let Some(disc) = metadata.disc
        && is_multi_disc
        && let Some(track_pos) = result.rfind('/')
    {
        let after_slash = &result[track_pos + 1..];
        result = format!("{}/Disc {}/{}", &result[..track_pos], disc, after_slash);
    }

    if let Some(genre) = &metadata.genre {
        result = result.replace("{genre}", &sanitize_metadata_value(genre, config));
    } else if template.contains("{genre}") {
        return None;
    }

    if template.contains("{filename}") {
        let filename = source_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        result = result.replace("{filename}", &filename);
    }

    if let Some(extension) = source_path.extension()
        && !result.ends_with(&format!(".{}", extension.to_string_lossy()))
    {
        result = format!("{}.{}", result, extension.to_string_lossy());
    }

    Some(result)
}

fn sanitize_path(path: &str, config: &Config) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let sanitized_parts: Vec<String> = parts
        .iter()
        .map(|part| {
            let mut sanitized_part = part.to_string();

            for (from, to) in &config.formatting.replace_chars {
                if from != "/" {
                    sanitized_part = sanitized_part.replace(from, to);
                }
            }

            if sanitized_part.len() > (config.formatting.max_filename_length as usize) {
                let max_len = config.formatting.max_filename_length as usize;
                if let Some(dot_pos) = sanitized_part.rfind('.') {
                    let name_part = &sanitized_part[..dot_pos];
                    let ext_part = &sanitized_part[dot_pos..];
                    if name_part.len() + ext_part.len() > max_len {
                        let available_for_name = max_len.saturating_sub(ext_part.len());
                        format!(
                            "{}{}",
                            &name_part[..available_for_name.min(name_part.len())],
                            ext_part
                        )
                    } else {
                        sanitized_part
                    }
                } else {
                    sanitized_part[..max_len.min(sanitized_part.len())].to_string()
                }
            } else {
                sanitized_part
            }
        })
        .collect();

    sanitized_parts.join("/")
}

fn handle_duplicate_rename(
    target_path: &Path,
    metadata_key: &MetadataKey,
    used_metadata: &mut HashMap<MetadataKey, PathBuf>,
) -> PathBuf {
    let mut counter = 1;
    let stem = target_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let extension = target_path
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let parent = target_path.parent().unwrap_or(Path::new(""));

    loop {
        let new_filename = format!("{} ({}){}", stem, counter, extension);
        let new_path = parent.join(new_filename);

        let mut new_metadata_key = metadata_key.clone();
        new_metadata_key.title = format!("{} ({})", metadata_key.title, counter);

        if let Entry::Vacant(e) = used_metadata.entry(new_metadata_key) {
            e.insert(new_path.clone());
            return new_path;
        }
        counter += 1;
    }
}

fn create_metadata_key(metadata: &AudioMetadata) -> Option<MetadataKey> {
    let artist = if is_compilation(metadata) {
        metadata.artist.as_ref()
    } else {
        metadata.album_artist.as_ref().or(metadata.artist.as_ref())
    };

    let artist = artist?;
    let album = metadata.album.as_ref()?;
    let title = metadata.title.as_ref()?;

    Some(MetadataKey {
        artist: artist.clone(),
        album: album.clone(),
        title: title.clone(),
        track: metadata.track,
        disc: metadata.disc,
    })
}

fn remove_pattern_case_insensitive(value: &str, pattern: &str) -> String {
    if pattern.is_empty() {
        return value.to_string();
    }

    let lower_value = value.to_lowercase();
    let lower_pattern = pattern.to_lowercase();
    let mut result = String::with_capacity(value.len());
    let mut last_end = 0;

    for (start, _) in lower_value.match_indices(&lower_pattern) {
        if start < last_end {
            continue;
        }
        result.push_str(&value[last_end..start]);
        last_end = start + pattern.len();
    }
    result.push_str(&value[last_end..]);

    result
}

fn sanitize_metadata_value(value: &str, config: &Config) -> String {
    let mut sanitized = value.to_string();
    for (from, to) in &config.formatting.replace_chars {
        sanitized = sanitized.replace(from, to);
    }
    sanitized
}

fn normalize_metadata_value(value: &str, config: &Config) -> String {
    let mut normalized = value.to_string();
    for pattern in &config.formatting.normalize_patterns {
        normalized = remove_pattern_case_insensitive(&normalized, pattern);
    }
    normalized.trim().to_string()
}

fn normalize_metadata(metadata: &AudioMetadata, config: &Config) -> AudioMetadata {
    let mut normalized = metadata.clone();
    normalized.title = metadata
        .title
        .as_deref()
        .map(|v| normalize_metadata_value(v, config));
    normalized.artist = metadata
        .artist
        .as_deref()
        .map(|v| normalize_metadata_value(v, config));
    normalized.album = metadata
        .album
        .as_deref()
        .map(|v| normalize_metadata_value(v, config));
    normalized.album_artist = metadata
        .album_artist
        .as_deref()
        .map(|v| normalize_metadata_value(v, config));
    normalized.genre = metadata
        .genre
        .as_deref()
        .map(|v| normalize_metadata_value(v, config));
    normalized
}

fn write_normalized_tags(
    path: &Path,
    original: &AudioMetadata,
    normalized: &AudioMetadata,
) -> Result<(), Box<dyn std::error::Error>> {
    let title_changed = original.title != normalized.title;
    let artist_changed = original.artist != normalized.artist;
    let album_changed = original.album != normalized.album;
    let album_artist_changed = original.album_artist != normalized.album_artist;
    let genre_changed = original.genre != normalized.genre;

    if !(title_changed || artist_changed || album_changed || album_artist_changed || genre_changed)
    {
        return Ok(());
    }

    let mut tagged_file = lofty::read_from_path(path)?;
    if tagged_file.primary_tag().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged_file
        .primary_tag_mut()
        .expect("a primary tag was just ensured");

    if title_changed && let Some(ref v) = normalized.title {
        tag.insert_text(ItemKey::TrackTitle, v.clone());
    }
    if artist_changed && let Some(ref v) = normalized.artist {
        tag.insert_text(ItemKey::TrackArtist, v.clone());
    }
    if album_changed && let Some(ref v) = normalized.album {
        tag.insert_text(ItemKey::AlbumTitle, v.clone());
    }
    if album_artist_changed && let Some(ref v) = normalized.album_artist {
        tag.insert_text(ItemKey::AlbumArtist, v.clone());
    }
    if genre_changed && let Some(ref v) = normalized.genre {
        tag.insert_text(ItemKey::Genre, v.clone());
    }

    tag.save_to_path(path, WriteOptions::default())?;

    Ok(())
}

fn primary_artist(name: &str) -> &str {
    let lower = name.to_ascii_lowercase();
    let end = [
        ";",
        " feat.",
        " feat ",
        " ft.",
        " ft ",
        " featuring ",
        " (feat",
        " (ft",
    ]
    .iter()
    .filter_map(|sep| lower.find(sep))
    .min()
    .unwrap_or(name.len());

    let primary = name[..end].trim();
    if primary.is_empty() {
        name.trim()
    } else {
        primary
    }
}

fn is_compilation(metadata: &AudioMetadata) -> bool {
    if let Some(album_artist) = &metadata.album_artist {
        album_artist.to_lowercase() == "various artists"
    } else {
        false
    }
}

fn cleanup_empty_dirs(dir: &Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.path().is_dir() {
                cleanup_empty_dirs(&entry.path());
            }
        }
    }

    if let Ok(entries) = fs::read_dir(dir) {
        let is_empty = entries.flatten().count() == 0;
        if is_empty && dir != Path::new("") {
            let _ = fs::remove_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_artist_cuts_features_but_keeps_acts() {
        assert_eq!(primary_artist("Travie McCoy Feat. Sia"), "Travie McCoy");
        assert_eq!(
            primary_artist("Fish & Chips feat. Lisa & Claire"),
            "Fish & Chips"
        );
        assert_eq!(primary_artist("20 Fingers; Roula"), "20 Fingers");
        assert_eq!(primary_artist("Earth Wind and Fire"), "Earth Wind and Fire");
        assert_eq!(primary_artist("Lo & Leduc"), "Lo & Leduc");
        assert_eq!(primary_artist("Tyler, The Creator"), "Tyler, The Creator");
    }

    #[test]
    fn artist_only_files_go_into_artist_folder() {
        let config = Config::default();
        let meta = AudioMetadata {
            title: None,
            artist: Some("Pumuckl".to_string()),
            album: None,
            album_artist: None,
            year: None,
            genre: None,
            track: None,
            disc: None,
        };
        let path = generate_target_path(
            Path::new("/m/01-A Pumuckl - En Kobold.mp3"),
            &meta,
            &config,
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(path, PathBuf::from("Pumuckl/01-A Pumuckl - En Kobold.mp3"));
    }

    #[test]
    fn no_metadata_still_falls_back() {
        let config = Config::default();
        let meta = AudioMetadata {
            title: None,
            artist: None,
            album: None,
            album_artist: None,
            year: None,
            genre: None,
            track: Some(3),
            disc: None,
        };
        assert!(
            generate_target_path(Path::new("/m/a.mp3"), &meta, &config, &HashSet::new()).is_none()
        );
    }
}
