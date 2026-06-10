use crate::scan::{collect_music_paths, is_music_file};

use ebur128::{EbuR128, Mode};
use indicatif::{ProgressBar, ProgressStyle};
use lofty::config::WriteOptions;
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag};
use rayon::prelude::*;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoderOptions, CODEC_ID_NULL_AUDIO};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::formats::probe::Hint;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use tracing::{debug, info, warn};

struct TrackAnalysis {
    loudness: f64,
    peak: f64,
    state: EbuR128,
}

pub fn apply_replaygain(path: &Path) -> Result<(), Box<dyn Error>> {
    let files: Vec<PathBuf> = if path.is_file() {
        if !is_music_file(path) {
            return Err(format!("{} is not a supported audio file", path.display()).into());
        }
        vec![path.to_path_buf()]
    } else {
        collect_music_paths(path)
    };

    if files.is_empty() {
        println!("No music files found to process.");
        return Ok(());
    }

    let mut albums: Vec<(PathBuf, Vec<PathBuf>)> = Vec::new();
    for file in files {
        let parent = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));

        if let Some(entry) = albums.iter_mut().find(|(p, _)| *p == parent) {
            entry.1.push(file);
        } else {
            albums.push((parent, vec![file]));
        }
    }

    for (_, album_files) in &mut albums {
        album_files.sort_by(|a, b| {
            let mtime_a = std::fs::metadata(a)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            let mtime_b = std::fs::metadata(b)
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            mtime_b.cmp(&mtime_a)
        });
    }

    albums.sort_by(|a, b| {
        let mtime_a =
            a.1.first()
                .and_then(|f| std::fs::metadata(f).ok())
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
        let mtime_b =
            b.1.first()
                .and_then(|f| std::fs::metadata(f).ok())
                .and_then(|m| m.modified().ok())
                .unwrap_or(SystemTime::UNIX_EPOCH);
        mtime_b.cmp(&mtime_a)
    });

    let total: usize = albums.iter().map(|(_, f)| f.len()).sum();
    let mut skipped_albums = 0usize;

    info!(
        "computing ReplayGain for {} files across {} album(s)",
        total,
        albums.len()
    );
    println!(
        "  Processing {} files across {} album(s) using {} threads",
        total,
        albums.len(),
        rayon::current_num_threads()
    );

    let start = Instant::now();

    let pb = Arc::new(ProgressBar::new(total as u64));
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")?
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let processed = Arc::new(Mutex::new(0usize));
    let failed = Arc::new(Mutex::new(0usize));

    for (album_dir, album_files) in &albums {
        let needs_processing = album_files.iter().any(|f| !has_replaygain_tags(f));
        if !needs_processing {
            skipped_albums += 1;
            for _ in album_files {
                pb.inc(1);
            }
            continue;
        }

        let analyses: Vec<(PathBuf, TrackAnalysis)> = album_files
            .par_iter()
            .filter_map(|file| {
                if let Some(name) = file.file_name() {
                    pb.set_message(name.to_string_lossy().to_string());
                }

                match analyze_track(file) {
                    Ok(analysis) => {
                        pb.inc(1);
                        Some((file.clone(), analysis))
                    }
                    Err(err) => {
                        warn!("failed to analyse {}: {}", file.display(), err);
                        eprintln!("  Failed to analyse {}: {}", file.display(), err);
                        *failed.lock().unwrap() += 1;
                        pb.inc(1);
                        None
                    }
                }
            })
            .collect();

        if analyses.is_empty() {
            continue;
        }

        let states: Vec<&EbuR128> = analyses.iter().map(|(_, a)| &a.state).collect();
        let album_loudness =
            EbuR128::loudness_global_multiple(states.into_iter()).unwrap_or(f64::NEG_INFINITY);
        let album_gain = gain_from_loudness(album_loudness);
        let album_peak = analyses.iter().map(|(_, a)| a.peak).fold(0.0_f64, f64::max);

        debug!(
            "album {} -> gain {:.2} dB, peak {:.6}",
            album_dir.display(),
            album_gain,
            album_peak
        );

        for (file, analysis) in &analyses {
            let track_gain = gain_from_loudness(analysis.loudness);
            match write_replaygain_tags(file, track_gain, analysis.peak, album_gain, album_peak) {
                Ok(()) => {
                    debug!(
                        "{} -> track {:.2} dB / {:.6}, album {:.2} dB / {:.6}",
                        file.display(),
                        track_gain,
                        analysis.peak,
                        album_gain,
                        album_peak
                    );
                    *processed.lock().unwrap() += 1;
                }
                Err(err) => {
                    warn!("failed to write tags to {}: {}", file.display(), err);
                    eprintln!("  Failed to write tags to {}: {}", file.display(), err);
                    *failed.lock().unwrap() += 1;
                }
            }
        }
    }

    pb.finish_and_clear();

    let duration = start.elapsed();
    let processed = *processed.lock().unwrap();
    let failed = *failed.lock().unwrap();

    info!(
        "tagged {} files in {:.2}s ({} failed, {} albums skipped)",
        processed,
        duration.as_secs_f64(),
        failed,
        skipped_albums
    );
    println!(
        "  {} files tagged in {:.2}s",
        processed,
        duration.as_secs_f64()
    );
    if skipped_albums > 0 {
        println!("  {} album(s) skipped (already tagged)", skipped_albums);
    }
    if failed > 0 {
        println!("  {} files failed", failed);
    }

    Ok(())
}

fn gain_from_loudness(loudness: f64) -> f64 {
    if loudness.is_finite() {
        -18 as f64 - loudness
    } else {
        0.0
    }
}

fn has_replaygain_tags(path: &Path) -> bool {
    let Ok(tagged_file) = lofty::read_from_path(path) else {
        return false;
    };
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return false;
    };
    let has_track_gain = tag
        .get_string(ItemKey::ReplayGainTrackGain)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_track_peak = tag
        .get_string(ItemKey::ReplayGainTrackPeak)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_album_gain = tag
        .get_string(ItemKey::ReplayGainAlbumGain)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let has_album_peak = tag
        .get_string(ItemKey::ReplayGainAlbumPeak)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    has_track_gain && has_track_peak && has_album_gain && has_album_peak
}

fn analyze_track(path: &Path) -> Result<TrackAnalysis, Box<dyn Error>> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let mut format_reader = symphonia::default::get_probe().probe(
        &hint,
        mss,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;

    let track = format_reader
        .tracks()
        .iter()
        .find(|t| {
            matches!(
                &t.codec_params,
                Some(CodecParameters::Audio(cp)) if cp.codec != CODEC_ID_NULL_AUDIO
            )
        })
        .ok_or("no decodable audio track found")?;

    let audio_params = track
        .codec_params
        .as_ref()
        .and_then(|cp| cp.audio())
        .ok_or("not an audio track")?;

    let track_id = track.id;
    let channels = audio_params
        .channels
        .as_ref()
        .ok_or("unknown channel layout")?
        .count();
    let sample_rate = audio_params.sample_rate.ok_or("unknown sample rate")?;

    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(audio_params, &AudioDecoderOptions::default())?;

    let mut ebu = EbuR128::new(channels as u32, sample_rate, Mode::I | Mode::TRUE_PEAK)?;
    let mut samples: Vec<f32> = Vec::new();

    loop {
        let packet = match format_reader.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(err) => return Err(err.into()),
        };

        if packet.track_id != track_id {
            continue;
        }

        match decoder.decode(&packet) {
            Ok(decoded) => {
                samples.clear();
                decoded.copy_to_vec_interleaved(&mut samples);
                ebu.add_frames_f32(&samples)?;
            }
            Err(SymphoniaError::IoError(_)) | Err(SymphoniaError::DecodeError(_)) => continue,
            Err(err) => return Err(err.into()),
        }
    }

    let loudness = ebu.loudness_global()?;
    let mut peak = 0.0_f64;
    for ch in 0..channels {
        peak = peak.max(ebu.true_peak(ch as u32)?);
    }

    Ok(TrackAnalysis {
        loudness,
        peak,
        state: ebu,
    })
}

fn write_replaygain_tags(
    path: &Path,
    track_gain: f64,
    track_peak: f64,
    album_gain: f64,
    album_peak: f64,
) -> Result<(), Box<dyn Error>> {
    let mut tagged_file = lofty::read_from_path(path)?;

    if tagged_file.primary_tag().is_none() {
        let tag_type = tagged_file.primary_tag_type();
        tagged_file.insert_tag(Tag::new(tag_type));
    }
    let tag = tagged_file
        .primary_tag_mut()
        .expect("a primary tag was just ensured");

    tag.insert_text(
        ItemKey::ReplayGainTrackGain,
        format!("{:.2} dB", track_gain),
    );
    tag.insert_text(ItemKey::ReplayGainTrackPeak, format!("{:.6}", track_peak));
    tag.insert_text(
        ItemKey::ReplayGainAlbumGain,
        format!("{:.2} dB", album_gain),
    );
    tag.insert_text(ItemKey::ReplayGainAlbumPeak, format!("{:.6}", album_peak));

    tag.save_to_path(path, WriteOptions::default())?;

    Ok(())
}
