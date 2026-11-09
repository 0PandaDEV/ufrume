use crate::scan::{collect_music_paths, is_music_file};

use crate::tag_manual;
use crate::tag_ui;

use console::{Term, truncate_str};
use image::DynamicImage;
use indicatif::{ProgressBar, ProgressStyle};
use lofty::TextEncoding;
use lofty::config::WriteOptions;
use lofty::id3::v2::{ExtendedTextFrame, Frame, Id3v2Tag};
use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
use lofty::ogg::tag::VorbisComments;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag, TagType};
use rayon::prelude::*;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use std::collections::{HashMap, HashSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::thread::sleep;
use std::time::{Duration, SystemTime};
use tracing::{debug, warn};
use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

const OMM_API: &str = "https://api.vleer.app/metadata/v1";
const MAX_ATTEMPTS: u32 = 3;
const SERVER_ERROR_ATTEMPTS: u32 = 2;
const WORKERS: usize = 16;
const PROGRESS_PREFIX_WIDTH: usize = 75;
const MAX_PARAM_CHARS: usize = 256;
const COVER_PIXELS: &str = "1000";
const SONG_ID_KEY: &str = "OMM_SONG_ID";
const ALBUM_ID_KEY: &str = "OMM_ALBUM_ID";
const ARTIST_ID_KEY: &str = "OMM_ARTIST_ID";
const COVER_CACHE: usize = 16;
const THUMB_CACHE: usize = 64;
const THUMB_PIXELS: u32 = 160;
const PREVIEW_PIXELS: &str = "300";
const SEARCH_LIMIT: u32 = 10;

#[derive(Default)]
pub(crate) struct ExistingMeta {
    pub(crate) title: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) duration_ms: Option<u128>,
}

#[derive(Debug, Deserialize)]
struct OmmSingle<T> {
    data: T,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OmmList<T> {
    pub(crate) data: Vec<T>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OmmSong {
    pub(crate) id: String,
    pub(crate) attributes: OmmSongAttributes,
    pub(crate) relationships: Option<OmmSongRelationships>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmmSongAttributes {
    pub(crate) name: String,
    pub(crate) album_name: Option<String>,
    #[serde(default)]
    pub(crate) artist_names: Vec<String>,
    pub(crate) isrc: Option<String>,
    pub(crate) track_number: Option<u32>,
    pub(crate) disc_number: Option<u32>,
    pub(crate) genres: Option<Vec<String>>,
    pub(crate) release_date: Option<String>,
    pub(crate) artwork_url: Option<String>,
    pub(crate) duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OmmSongRelationships {
    pub(crate) albums: Option<OmmList<OmmAlbum>>,
    pub(crate) artists: Option<OmmList<OmmArtist>>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OmmAlbum {
    pub(crate) id: String,
    pub(crate) attributes: OmmAlbumAttributes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmmAlbumAttributes {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) artist_names: Vec<String>,
    pub(crate) release_date: Option<String>,
    pub(crate) artwork_url: Option<String>,
    pub(crate) track_count: Option<u32>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OmmArtist {
    pub(crate) id: String,
    pub(crate) attributes: OmmArtistAttributes,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OmmArtistAttributes {
    pub(crate) name: String,
    pub(crate) artwork_url: Option<String>,
    pub(crate) genres: Option<Vec<String>>,
}

#[derive(Clone, Default)]
pub(crate) struct SongHint {
    pub(crate) artist_id: Option<String>,
    pub(crate) album_id: Option<String>,
    pub(crate) artist: Option<String>,
    pub(crate) album: Option<String>,
    pub(crate) duration_ms: Option<u128>,
}

pub(crate) struct Probe {
    pub(crate) variants: Vec<String>,
    pub(crate) hint: SongHint,
}

pub(crate) struct Suggestion<T> {
    pub(crate) item: T,
    pub(crate) votes: usize,
    pub(crate) total: usize,
}

impl<T> Suggestion<T> {
    fn new(item: T, total: usize) -> Self {
        Self {
            item,
            votes: 0,
            total,
        }
    }
}

#[derive(Default)]
pub(crate) struct Discovery {
    pub(crate) albums: Vec<Suggestion<OmmAlbum>>,
    pub(crate) artists: Vec<Suggestion<OmmArtist>>,
}

fn artist_fits(song: &OmmSong, artist: Option<&str>) -> bool {
    match non_empty(artist) {
        Some(artist) if !song.attributes.artist_names.is_empty() && !is_generic_artist(artist) => {
            song.attributes
                .artist_names
                .iter()
                .any(|name| names_match(name, artist) || names_match(artist, name))
        }
        _ => true,
    }
}

#[derive(Debug, Clone)]
pub struct TagCandidate {
    pub title: String,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_artists: Vec<String>,
    pub date: Option<String>,
    pub track: Option<u32>,
    pub disc: Option<u32>,
    pub genre: Option<String>,
    pub isrc: Option<String>,
    pub artwork: Option<String>,
    pub song_id: String,
    pub album_id: Option<String>,
    pub artist_ids: Vec<String>,
}

pub struct Report {
    pub candidate: TagCandidate,
    pub changed: Vec<&'static str>,
    pub wrote: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy)]
pub enum SkipReason {
    AlreadyTagged,
    NoTags,
    NoTitle,
    NoMatch,
}

impl SkipReason {
    pub fn is_unmatched(self) -> bool {
        matches!(self, SkipReason::NoMatch)
    }

    pub fn label(self) -> &'static str {
        match self {
            SkipReason::AlreadyTagged => "already tagged",
            SkipReason::NoTags => "no usable tags",
            SkipReason::NoTitle => "no title",
            SkipReason::NoMatch => "no match",
        }
    }
}

enum Outcome {
    Tagged(Box<Report>),
    Skipped(SkipReason),
}

pub enum Event {
    Started(String),
    Tagged {
        rel: String,
        report: Box<Report>,
        cover: Option<Arc<DynamicImage>>,
    },
    Skipped {
        rel: String,
        reason: SkipReason,
    },
    Failed {
        rel: String,
        error: String,
    },
    Finished,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Counts {
    pub tagged: usize,
    pub skipped: usize,
    pub unmatched: usize,
    pub failed: usize,
}

type Query = Vec<(&'static str, String)>;
type ApiResult<T> = Result<T, Box<dyn std::error::Error>>;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Scope<'a> {
    artist_id: Option<&'a str>,
    album_id: Option<&'a str>,
}

impl Scope<'_> {
    fn push(&self, query: &mut Query) {
        if let Some(id) = self.artist_id {
            query.push(("artist_id", id.to_string()));
        }
        if let Some(id) = self.album_id {
            query.push(("album_id", id.to_string()));
        }
    }
}

pub(crate) struct Resolver {
    client: reqwest::blocking::Client,
    artists: Mutex<HashMap<String, Option<OmmArtist>>>,
    albums: Mutex<HashMap<String, Option<OmmAlbum>>>,
    covers: Mutex<HashMap<String, Option<Arc<Vec<u8>>>>>,
}

pub(crate) fn has_omm_tags(path: &Path) -> bool {
    let Ok(tagged_file) = lofty::read_from_path(path) else {
        return false;
    };
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return false;
    };
    read_omm_song_id(tag).is_some_and(|id| {
        let id = id.trim();
        valid_id(id, "song") || is_bare_id(id)
    })
}

fn is_bare_id(id: &str) -> bool {
    (1..=18).contains(&id.len()) && !id.starts_with('0') && id.bytes().all(|b| b.is_ascii_digit())
}

fn bare_id(id: &str) -> &str {
    id.rsplit(':').next().unwrap_or(id)
}

fn omm_ids(candidate: &TagCandidate) -> Vec<(&'static str, String)> {
    let mut ids = Vec::new();
    if !candidate.song_id.is_empty() {
        ids.push((SONG_ID_KEY, bare_id(&candidate.song_id).to_string()));
    }
    if let Some(ref id) = candidate.album_id {
        ids.push((ALBUM_ID_KEY, bare_id(id).to_string()));
    }
    if !candidate.artist_ids.is_empty() {
        let artists: Vec<&str> = candidate.artist_ids.iter().map(|id| bare_id(id)).collect();
        ids.push((ARTIST_ID_KEY, artists.join("; ")));
    }
    ids
}

fn read_omm_song_id(tag: &Tag) -> Option<String> {
    read_omm_key(tag, SONG_ID_KEY)
}

fn read_omm_key(tag: &Tag, key: &str) -> Option<String> {
    match tag.tag_type() {
        TagType::Id3v2 => Id3v2Tag::from(tag.clone())
            .into_iter()
            .find_map(|frame| match frame {
                Frame::UserText(f) if f.description.eq_ignore_ascii_case(key) => {
                    Some(f.content.to_string())
                }
                _ => None,
            }),
        TagType::VorbisComments => VorbisComments::from(tag.clone())
            .get(key)
            .map(str::to_string),
        TagType::Mp4Ilst => Ilst::from(tag.clone()).into_iter().find_map(|atom| {
            match (atom.ident(), atom.data().next()) {
                (AtomIdent::Freeform { name, .. }, Some(AtomData::UTF8(v)))
                    if name.eq_ignore_ascii_case(key) =>
                {
                    Some(v.clone())
                }
                _ => None,
            }
        }),
        _ => None,
    }
}

pub(crate) fn missing_fields(path: &Path) -> Result<Vec<&'static str>, String> {
    let tagged_file = lofty::read_from_path(path).map_err(|e| e.to_string())?;
    let Some(tag) = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
    else {
        return Ok(vec!["all"]);
    };

    let blank = |v: Option<std::borrow::Cow<'_, str>>| v.is_none_or(|s| s.trim().is_empty());
    let item_blank = |key: ItemKey| tag.get_string(key).is_none_or(|s| s.trim().is_empty());
    let id_blank = |key: &str| read_omm_key(tag, key).is_none_or(|s| s.trim().is_empty());

    let mut missing = Vec::new();
    if blank(tag.title()) {
        missing.push("title");
    }
    if blank(tag.artist()) {
        missing.push("artist");
    }
    if blank(tag.album()) {
        missing.push("album");
    }
    if item_blank(ItemKey::AlbumArtist) {
        missing.push("album_artist");
    }
    if item_blank(ItemKey::RecordingDate) && item_blank(ItemKey::Year) {
        missing.push("date");
    }
    if tag.track().is_none() {
        missing.push("track");
    }
    if tag.pictures().is_empty() {
        missing.push("cover");
    }
    if id_blank(SONG_ID_KEY) {
        missing.push("song_id");
    }
    if id_blank(ALBUM_ID_KEY) {
        missing.push("album_id");
    }
    if id_blank(ARTIST_ID_KEY) {
        missing.push("artist_id");
    }
    Ok(missing)
}

pub(crate) fn check_music_files(path: &Path) -> Result<usize, Box<dyn std::error::Error>> {
    let files = if path.is_file() {
        if !is_music_file(path) {
            return Err(format!("not a music file: {}", path.display()).into());
        }
        vec![path.to_path_buf()]
    } else {
        collect_music_paths(path)
    };

    let mut results: Vec<(PathBuf, Result<Vec<&'static str>, String>)> = files
        .into_par_iter()
        .map(|p| {
            let r = missing_fields(&p);
            (p, r)
        })
        .collect();
    results.sort_by(|a, b| a.0.cmp(&b.0));

    let total = results.len();
    let mut bad = 0;
    for (p, r) in &results {
        match r {
            Ok(m) if m.is_empty() => {}
            Ok(m) => {
                bad += 1;
                println!("{}  [{}]", p.display(), m.join(", "));
            }
            Err(e) => {
                bad += 1;
                println!("{}  [unreadable: {}]", p.display(), e);
            }
        }
    }
    eprintln!("{}/{} not fully tagged", bad, total);
    Ok(bad)
}

fn save_with_ids(
    tag: Tag,
    ids: &[(&'static str, String)],
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let options = WriteOptions::default();
    match tag.tag_type() {
        TagType::Id3v2 => {
            let mut native = Id3v2Tag::from(tag);
            for (key, value) in ids {
                native.insert(Frame::UserText(ExtendedTextFrame::new(
                    TextEncoding::UTF8,
                    key.to_string(),
                    value.clone(),
                )));
            }
            native.save_to_path(path, options)?;
        }
        TagType::VorbisComments => {
            let mut native = VorbisComments::from(tag);
            for (key, value) in ids {
                native.insert(key.to_string(), value.clone());
            }
            native.save_to_path(path, options)?;
        }
        TagType::Mp4Ilst => {
            let mut native = Ilst::from(tag);
            for (key, value) in ids {
                native.insert(Atom::new(
                    AtomIdent::Freeform {
                        mean: "com.apple.iTunes".into(),
                        name: key.to_string().into(),
                    },
                    AtomData::UTF8(value.clone()),
                ));
            }
            native.save_to_path(path, options)?;
        }
        _ => tag.save_to_path(path, options)?,
    }
    Ok(())
}

pub(crate) fn read_existing_metadata(path: &Path) -> Option<ExistingMeta> {
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

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn valid_id(id: &str, kind: &str) -> bool {
    id.strip_prefix("omm:")
        .and_then(|rest| rest.strip_prefix(kind))
        .and_then(|rest| rest.strip_prefix(':'))
        .is_some_and(is_bare_id)
}

fn artist_queries(artist: &str) -> Vec<String> {
    let full = artist.trim().to_string();
    let lower = full.to_ascii_lowercase();
    let end = [",", ";", "&", "/", " feat", " ft.", " ft ", " x "]
        .iter()
        .filter_map(|sep| lower.find(sep))
        .min()
        .unwrap_or(full.len());
    let first = full[..end].trim().to_string();

    let mut queries = vec![full];
    if !first.is_empty() && queries[0] != first {
        queries.push(first);
    }
    queries
}

fn get_json<T: DeserializeOwned>(
    client: &reqwest::blocking::Client,
    path: &str,
    query: &Query,
) -> ApiResult<Option<T>> {
    let url = format!("{}{}", OMM_API, path);

    let mut last_status = None;

    for attempt in 1..=MAX_ATTEMPTS {
        let response = client.get(&url).query(query).send()?;
        let status = response.status();

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
            last_status = Some(status);
            let limit = if status.is_server_error() {
                SERVER_ERROR_ATTEMPTS
            } else {
                MAX_ATTEMPTS
            };
            if attempt >= limit {
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

        return Ok(Some(response.json()?));
    }

    let code = last_status.map(|s| s.as_u16()).unwrap_or_default();
    Err(format!("API unavailable ({})", code).into())
}

fn fit(text: &str, reserved: usize) -> String {
    let width = Term::stdout().size().1 as usize;
    truncate_str(text, width.saturating_sub(reserved).max(20), "…").into_owned()
}

fn fold(text: &str) -> String {
    text.nfd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(|c| match c {
            'ß' => vec!['s', 's'],
            'æ' | 'Æ' => vec!['a', 'e'],
            'œ' | 'Œ' => vec!['o', 'e'],
            'ø' | 'Ø' => vec!['o'],
            'ł' | 'Ł' => vec!['l'],
            'đ' | 'Đ' => vec!['d'],
            other => other.to_lowercase().collect(),
        })
        .filter(|c| c.is_alphanumeric())
        .collect()
}

fn fold_tokens(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(fold)
        .filter(|t| !t.is_empty())
        .collect()
}

pub(crate) fn names_match(a: &str, b: &str) -> bool {
    let (fa, fb) = (fold(a), fold(b));
    if fa.is_empty() || fb.is_empty() {
        return false;
    }
    if fa == fb {
        return true;
    }
    let (short, long) = if fa.len() <= fb.len() {
        (&fa, &fb)
    } else {
        (&fb, &fa)
    };
    if short.len() >= 3 && long.contains(short.as_str()) {
        return true;
    }
    let (ta, tb) = (fold_tokens(a), fold_tokens(b));
    let union = ta.union(&tb).count() as f64;
    union > 0.0 && ta.intersection(&tb).count() as f64 / union >= 0.75
}

fn is_generic_artist(name: &str) -> bool {
    matches!(
        fold(name).as_str(),
        "variousartists"
            | "various"
            | "va"
            | "vvaa"
            | "unknownartist"
            | "unknown"
            | "artistunknown"
            | "verschiedeneinterpreten"
            | "verschiedenekunstler"
            | "artistesdivers"
            | "variosartistas"
    )
}

fn prioritize_artist(names: &mut [String], preferred: &str) {
    if let Some(pos) = names
        .iter()
        .position(|n| names_match(n, preferred) || names_match(preferred, n))
    {
        names[..=pos].rotate_right(1);
    }
}

fn song_matches(song: &OmmSong, existing: &ExistingMeta, title: &str) -> bool {
    if !names_match(&song.attributes.name, title) {
        return false;
    }

    match existing.artist.as_deref() {
        Some(artist) if !song.attributes.artist_names.is_empty() && !is_generic_artist(artist) => {
            song.attributes
                .artist_names
                .iter()
                .any(|name| names_match(name, artist) || names_match(artist, name))
        }
        _ => true,
    }
}

fn song_to_candidate(
    song: OmmSong,
    album_id: Option<&str>,
    artist_id: Option<&str>,
    existing_album: Option<&str>,
) -> TagCandidate {
    let song_id = song.id;
    let attrs = song.attributes;
    let relationships = song.relationships;
    let artist_ids: Vec<String> = relationships
        .as_ref()
        .and_then(|r| r.artists.as_ref())
        .map(|a| a.data.iter().map(|a| a.id.clone()).collect::<Vec<_>>())
        .filter(|ids| !ids.is_empty())
        .unwrap_or_else(|| artist_id.map(str::to_string).into_iter().collect());
    let albums = relationships
        .and_then(|r| r.albums)
        .map(|a| a.data)
        .unwrap_or_default();

    let position = album_id
        .and_then(|id| albums.iter().position(|a| a.id == id))
        .or_else(|| {
            existing_album.and_then(|local| {
                albums
                    .iter()
                    .position(|a| names_match(&a.attributes.name, local))
            })
        })
        .unwrap_or(0);

    let (album_name, album_artists, album_date, album_artwork, matched_album_id) =
        match albums.into_iter().nth(position) {
            Some(a) => (
                Some(a.attributes.name),
                a.attributes.artist_names,
                a.attributes.release_date,
                a.attributes.artwork_url,
                Some(a.id),
            ),
            None => (None, Vec::new(), None, None, None),
        };

    TagCandidate {
        title: attrs.name,
        artists: attrs.artist_names,
        album: album_name.or(attrs.album_name),
        album_artists,
        date: attrs.release_date.or(album_date),
        track: attrs.track_number,
        disc: attrs.disc_number,
        genre: attrs.genres.filter(|g| !g.is_empty()).map(|g| g.join("; ")),
        isrc: attrs.isrc,
        artwork: album_artwork.or(attrs.artwork_url),
        song_id,
        album_id: matched_album_id,
        artist_ids,
    }
}

fn thumbnail(bytes: &[u8]) -> ApiResult<DynamicImage> {
    let image = image::load_from_memory(bytes)?;
    let side = image.width().min(image.height());
    let (x, y) = ((image.width() - side) / 2, (image.height() - side) / 2);
    Ok(image.crop_imm(x, y, side, side).resize_exact(
        THUMB_PIXELS,
        THUMB_PIXELS,
        image::imageops::FilterType::Triangle,
    ))
}

fn sized_cover_url(url: &str) -> String {
    sized_cover_url_px(url, COVER_PIXELS)
}

fn sized_cover_url_px(url: &str, px: &str) -> String {
    let url = url.replace("{w}", px).replace("{h}", px);
    let Some((base, name)) = url.rsplit_once('/') else {
        return url;
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let sized = name.split_once('x').and_then(|(w, rest)| {
        let end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        (digits(w) && digits(&rest[..end])).then(|| &rest[end..])
    });
    match sized {
        Some(suffix) => format!("{base}/{px}x{px}{suffix}"),
        None => url,
    }
}

fn has_front_cover(path: &Path) -> bool {
    let Ok(tagged_file) = lofty::read_from_path(path) else {
        return false;
    };
    tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .is_some_and(|tag| {
            tag.pictures()
                .iter()
                .any(|p| p.pic_type() == PictureType::CoverFront)
        })
}

fn front_cover(bytes: &[u8]) -> Option<Picture> {
    let mut picture = Picture::from_reader(&mut &bytes[..]).ok()?;
    if !matches!(picture.mime_type(), Some(MimeType::Jpeg | MimeType::Png)) {
        return None;
    }
    picture.set_pic_type(PictureType::CoverFront);
    Some(picture)
}

fn report_for(existing: &ExistingMeta, candidate: TagCandidate) -> Report {
    let differs = |old: Option<&str>, new: &str| old.map(str::trim) != Some(new.trim());

    let mut changed: Vec<&'static str> = Vec::new();
    if differs(existing.title.as_deref(), &candidate.title) {
        changed.push("title");
    }
    if !candidate.artists.is_empty()
        && differs(existing.artist.as_deref(), &candidate.artists.join(", "))
    {
        changed.push("artist");
    }
    if let Some(ref album) = candidate.album
        && differs(existing.album.as_deref(), album)
    {
        changed.push("album");
    }

    let mut wrote: Vec<&'static str> = Vec::new();
    if candidate.date.is_some() {
        wrote.push("date");
    }
    if candidate.track.is_some() {
        wrote.push("track");
    }
    if candidate.disc.is_some() {
        wrote.push("disc");
    }
    if candidate.genre.is_some() {
        wrote.push("genre");
    }
    if candidate.isrc.is_some() {
        wrote.push("isrc");
    }
    wrote.push("omm ids");

    Report {
        candidate,
        changed,
        wrote,
    }
}

impl Resolver {
    pub(crate) fn new(client: reqwest::blocking::Client) -> Self {
        Self {
            client,
            artists: Mutex::new(HashMap::new()),
            albums: Mutex::new(HashMap::new()),
            covers: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn cover_bytes(&self, url: &str) -> Option<Arc<Vec<u8>>> {
        if let Some(cached) = self.covers.lock().unwrap().get(url) {
            return cached.clone();
        }

        let bytes = self
            .client
            .get(sized_cover_url(url))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map(|b| Arc::new(b.to_vec()))
            .map_err(|e| debug!("cover download failed: {}", e))
            .ok();

        let mut covers = self.covers.lock().unwrap();
        if covers.len() >= COVER_CACHE {
            covers.clear();
        }
        covers.insert(url.to_string(), bytes.clone());
        bytes
    }

    pub(crate) fn preview(&self, url: &str) -> Option<DynamicImage> {
        let bytes = self
            .client
            .get(sized_cover_url_px(url, PREVIEW_PIXELS))
            .send()
            .and_then(|r| r.error_for_status())
            .and_then(|r| r.bytes())
            .map_err(|e| debug!("preview download failed: {}", e))
            .ok()?;
        thumbnail(&bytes).ok()
    }

    fn search<T: DeserializeOwned>(&self, path: &str, mut query: Query) -> ApiResult<Vec<T>> {
        query.push(("limit", SEARCH_LIMIT.to_string()));
        Ok(get_json::<OmmList<T>>(&self.client, path, &query)?
            .map(|r| r.data)
            .unwrap_or_default())
    }

    pub(crate) fn search_artists(&self, name: &str) -> ApiResult<Vec<OmmArtist>> {
        self.search("/search/artist", vec![("name", truncate_param(name))])
    }

    pub(crate) fn search_albums(
        &self,
        name: &str,
        artist_id: Option<&str>,
    ) -> ApiResult<Vec<OmmAlbum>> {
        let mut query: Query = vec![("name", truncate_param(name))];
        if let Some(id) = artist_id.filter(|id| valid_id(id, "artist")) {
            query.push(("artist_id", id.to_string()));
        }
        self.search("/search/album", query)
    }

    pub(crate) fn search_songs(&self, name: &str, hint: &SongHint) -> ApiResult<Vec<OmmSong>> {
        let mut query: Query = vec![("name", truncate_param(name))];
        let album_id = hint.album_id.as_deref().filter(|id| valid_id(id, "album"));
        let artist_id = hint
            .artist_id
            .as_deref()
            .filter(|id| valid_id(id, "artist"));
        match (album_id, artist_id) {
            (Some(id), _) => query.push(("album_id", id.to_string())),
            (None, Some(id)) => query.push(("artist_id", id.to_string())),
            _ => {}
        }
        if artist_id.is_none()
            && let Some(artist) =
                non_empty(hint.artist.as_deref()).filter(|a| !is_generic_artist(a))
        {
            query.push(("artist", truncate_param(artist)));
        }
        if album_id.is_none()
            && let Some(album) = non_empty(hint.album.as_deref())
        {
            query.push(("album", truncate_param(album)));
        }
        if let Some(duration) = hint.duration_ms {
            query.push(("duration", duration.to_string()));
        }
        self.search("/search/song", query)
    }

    fn rank_songs(&self, variants: &[String], hint: &SongHint) -> ApiResult<Vec<OmmSong>> {
        let mut failure = None;
        for variant in variants {
            match self.search_songs(variant, hint) {
                Ok(songs) => {
                    let mut hits: Vec<OmmSong> = songs
                        .into_iter()
                        .filter(|song| names_match(&song.attributes.name, variant))
                        .filter(|song| artist_fits(song, hint.artist.as_deref()))
                        .collect();
                    if let Some(local) = hint.duration_ms {
                        hits.sort_by_key(|song| {
                            song.attributes
                                .duration_ms
                                .map_or(u128::MAX, |ms| u128::from(ms).abs_diff(local))
                        });
                    }
                    if !hits.is_empty() {
                        return Ok(hits);
                    }
                }
                Err(e) => failure = Some(e),
            }
        }
        match failure {
            Some(e) => Err(e),
            None => Ok(Vec::new()),
        }
    }

    pub(crate) fn match_song(
        &self,
        variants: &[String],
        hint: &SongHint,
    ) -> ApiResult<Option<OmmSong>> {
        Ok(self.rank_songs(variants, hint)?.into_iter().next())
    }

    pub(crate) fn discover(&self, probes: &[Probe]) -> Discovery {
        let hits: Vec<Vec<OmmSong>> = probes
            .par_iter()
            .map(|probe| {
                self.rank_songs(&probe.variants, &probe.hint)
                    .unwrap_or_default()
            })
            .collect();

        let mut albums: HashMap<String, Suggestion<OmmAlbum>> = HashMap::new();
        let mut artists: HashMap<String, Suggestion<OmmArtist>> = HashMap::new();
        for songs in &hits {
            let mut seen_albums = HashSet::new();
            let mut seen_artists = HashSet::new();
            for song in songs.iter().take(5) {
                let Some(relationships) = song.relationships.as_ref() else {
                    continue;
                };
                for album in relationships.albums.iter().flat_map(|l| l.data.iter()) {
                    if seen_albums.insert(album.id.clone()) {
                        albums
                            .entry(album.id.clone())
                            .or_insert_with(|| Suggestion::new(album.clone(), probes.len()))
                            .votes += 1;
                    }
                }
                for artist in relationships.artists.iter().flat_map(|l| l.data.iter()) {
                    if seen_artists.insert(artist.id.clone()) {
                        artists
                            .entry(artist.id.clone())
                            .or_insert_with(|| Suggestion::new(artist.clone(), probes.len()))
                            .votes += 1;
                    }
                }
            }
        }

        fn ranked<T>(
            map: HashMap<String, Suggestion<T>>,
            name: impl Fn(&T) -> &str,
        ) -> Vec<Suggestion<T>> {
            let mut list: Vec<Suggestion<T>> = map.into_values().collect();
            list.sort_by(|a, b| {
                b.votes
                    .cmp(&a.votes)
                    .then_with(|| name(&a.item).cmp(name(&b.item)))
            });
            list.truncate(SEARCH_LIMIT as usize);
            list
        }

        Discovery {
            albums: ranked(albums, |a| a.attributes.name.as_str()),
            artists: ranked(artists, |a| a.attributes.name.as_str()),
        }
    }

    fn resolve_artist(&self, artist: &str) -> ApiResult<Option<OmmArtist>> {
        let key = artist.trim().to_lowercase();
        if let Some(cached) = self.artists.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }

        let mut found = None;
        for name in artist_queries(artist) {
            let query: Query = vec![("name", truncate_param(&name))];
            let hit = get_json::<OmmSingle<OmmArtist>>(&self.client, "/identify/artist", &query)?
                .map(|r| r.data)
                .filter(|a| names_match(&a.attributes.name, artist));
            if hit.is_some() {
                found = hit;
                break;
            }
        }

        self.artists.lock().unwrap().insert(key, found.clone());
        Ok(found)
    }

    fn resolve_album(
        &self,
        album: &str,
        artist_name: Option<&str>,
        artist: Option<&OmmArtist>,
    ) -> ApiResult<Option<OmmAlbum>> {
        let key = format!(
            "{}\u{1f}{}",
            artist
                .map(|a| a.id.clone())
                .or_else(|| artist_name.map(|a| a.trim().to_lowercase()))
                .unwrap_or_default(),
            album.trim().to_lowercase()
        );
        if let Some(cached) = self.albums.lock().unwrap().get(&key) {
            return Ok(cached.clone());
        }

        let artist_id = artist
            .map(|a| a.id.as_str())
            .filter(|id| valid_id(id, "artist"));
        let scopes: Vec<Scope> = [
            Scope {
                artist_id,
                album_id: None,
            },
            Scope::default(),
        ]
        .into_iter()
        .fold(Vec::new(), |mut acc, scope| {
            if !acc.contains(&scope) {
                acc.push(scope);
            }
            acc
        });

        let mut found = None;
        let mut failure = None;
        for scope in scopes {
            let mut query: Query = vec![("name", truncate_param(album))];
            if scope.artist_id.is_none()
                && let Some(name) = non_empty(artist_name)
            {
                query.push(("artist", truncate_param(name)));
            }
            scope.push(&mut query);

            match get_json::<OmmSingle<OmmAlbum>>(&self.client, "/identify/album", &query) {
                Ok(hit) => {
                    let hit = hit
                        .map(|r| r.data)
                        .filter(|a| names_match(&a.attributes.name, album));
                    if hit.is_some() {
                        found = hit;
                        break;
                    }
                }
                Err(e) => {
                    debug!("album lookup failed: {}", e);
                    failure = Some(e);
                }
            }
        }

        if found.is_none()
            && let Some(e) = failure
        {
            return Err(e);
        }

        self.albums.lock().unwrap().insert(key, found.clone());
        Ok(found)
    }

    fn song_query(existing: &ExistingMeta, title: &str, scope: Scope) -> Query {
        let mut query: Query = vec![("name", truncate_param(title))];
        if let Some(artist) = non_empty(existing.artist.as_deref()) {
            query.push(("artist", truncate_param(artist)));
        }
        if let Some(album) = non_empty(existing.album.as_deref()) {
            query.push(("album", truncate_param(album)));
        }
        if let Some(duration) = existing.duration_ms {
            query.push(("duration", duration.to_string()));
        }
        scope.push(&mut query);
        query
    }

    fn resolve_song(
        &self,
        existing: &ExistingMeta,
        title: &str,
        album_id: Option<&str>,
    ) -> ApiResult<Option<OmmSong>> {
        let mut scopes: Vec<Scope> = Vec::new();
        if album_id.is_some() {
            scopes.push(Scope {
                artist_id: None,
                album_id,
            });
        }
        scopes.push(Scope::default());

        let mut failure = None;

        for scope in &scopes {
            let query = Self::song_query(existing, title, *scope);
            match get_json::<OmmSingle<OmmSong>>(&self.client, "/identify/song", &query) {
                Ok(hit) => {
                    if let Some(song) = hit.map(|r| r.data)
                        && song_matches(&song, existing, title)
                    {
                        return Ok(Some(song));
                    }
                }
                Err(e) => {
                    debug!("song identify failed: {}", e);
                    failure = Some(e);
                }
            }
        }

        let mut query = Self::song_query(existing, title, Scope::default());
        query.push(("limit", "10".to_string()));
        match get_json::<OmmList<OmmSong>>(&self.client, "/search/song", &query) {
            Ok(results) => {
                if let Some(song) = results
                    .map(|r| r.data)
                    .unwrap_or_default()
                    .into_iter()
                    .find(|song| song_matches(song, existing, title))
                {
                    return Ok(Some(song));
                }
            }
            Err(e) => {
                debug!("song search failed: {}", e);
                failure = Some(e);
            }
        }

        match failure {
            Some(e) => Err(e),
            None => Ok(None),
        }
    }
}

fn write_metadata_tags(
    path: &Path,
    candidate: &TagCandidate,
    cover: Option<&[u8]>,
) -> Result<bool, Box<dyn std::error::Error>> {
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

    let mut embedded = false;
    if let Some(picture) = cover.and_then(front_cover) {
        tag.remove_picture_type(PictureType::CoverFront);
        tag.push_picture(picture);
        embedded = true;
    }

    save_with_ids(tag.clone(), &omm_ids(candidate), path)?;

    Ok(embedded)
}

fn spawn_workers(
    files: Vec<PathBuf>,
    root: PathBuf,
    force: bool,
    covers: bool,
    resolver: Resolver,
    tx: Sender<Event>,
    stop: Arc<AtomicBool>,
) -> Vec<JoinHandle<()>> {
    let files = Arc::new(files);
    let root = Arc::new(root);
    let resolver = Arc::new(resolver);
    let thumbs: Arc<Mutex<HashMap<String, Option<Arc<DynamicImage>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let next = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(WORKERS));

    (0..WORKERS)
        .map(|_| {
            let (files, root, resolver, thumbs) = (
                files.clone(),
                root.clone(),
                resolver.clone(),
                thumbs.clone(),
            );
            let (next, active, stop, tx) = (next.clone(), active.clone(), stop.clone(), tx.clone());

            std::thread::spawn(move || {
                loop {
                    if stop.load(Ordering::Relaxed) {
                        break;
                    }
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(file) = files.get(index) else {
                        break;
                    };

                    let rel = file
                        .strip_prefix(root.as_path())
                        .unwrap_or(file.as_path())
                        .to_string_lossy()
                        .to_string();
                    if tx.send(Event::Started(rel.clone())).is_err() {
                        break;
                    }

                    let event = match process_file(file, force, &resolver) {
                        Ok(Outcome::Tagged(report)) => {
                            let cover = covers
                                .then(|| report.candidate.artwork.clone())
                                .flatten()
                                .and_then(|url| {
                                    if let Some(known) = thumbs.lock().unwrap().get(&url) {
                                        return known.clone();
                                    }
                                    let thumb = resolver
                                        .cover_bytes(&url)
                                        .and_then(|bytes| thumbnail(&bytes).ok())
                                        .map(Arc::new);
                                    let mut thumbs = thumbs.lock().unwrap();
                                    if thumbs.len() >= THUMB_CACHE {
                                        thumbs.clear();
                                    }
                                    thumbs.insert(url, thumb.clone());
                                    thumb
                                });
                            Event::Tagged { rel, report, cover }
                        }
                        Ok(Outcome::Skipped(reason)) => Event::Skipped { rel, reason },
                        Err(e) => {
                            warn!("failed to tag {}: {}", file.display(), e);
                            Event::Failed {
                                rel,
                                error: e.to_string(),
                            }
                        }
                    };

                    if tx.send(event).is_err() {
                        break;
                    }
                }

                if active.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let _ = tx.send(Event::Finished);
                }
            })
        })
        .collect()
}

fn run_plain(total: usize, rx: Receiver<Event>) -> Counts {
    let pb = ProgressBar::new(total as u64);
    pb.set_style(
        ProgressStyle::default_bar()
            .template("  [{bar:40.cyan/blue}] {pos}/{len} [{elapsed_precise}] {msg}")
            .expect("progress template is valid")
            .progress_chars("█▉▊▋▌▍▎▏  "),
    );

    let mut counts = Counts::default();
    for event in rx {
        match event {
            Event::Started(rel) => pb.set_message(fit(&rel, PROGRESS_PREFIX_WIDTH)),
            Event::Tagged { .. } => {
                counts.tagged += 1;
                pb.inc(1);
            }
            Event::Skipped { reason, .. } => {
                if reason.is_unmatched() {
                    counts.unmatched += 1;
                } else {
                    counts.skipped += 1;
                }
                pb.inc(1);
            }
            Event::Failed { rel, error } => {
                let name = Path::new(&rel)
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                pb.println(fit(&format!("  Failed: {} - {}", name, error), 0));
                counts.failed += 1;
                pb.inc(1);
            }
            Event::Finished => break,
        }
    }

    pb.finish_and_clear();
    counts
}

pub fn tag_music_files(
    path: &Path,
    force: bool,
    plain: bool,
    manual: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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

    if manual {
        if !std::io::stdout().is_terminal() {
            return Err("manual tagging needs an interactive terminal".into());
        }
        let counts = tag_manual::run(sorted_files, force, Arc::new(Resolver::new(client)))?;
        println!(
            "  {} files tagged, {} left untouched, {} failed",
            console::style(counts.tagged).green(),
            console::style(counts.skipped).dim(),
            console::style(counts.failed).red()
        );
        return Ok(());
    }

    let root = if path.is_file() {
        path.parent().unwrap_or(path).to_path_buf()
    } else {
        path.to_path_buf()
    };
    let total = sorted_files.len();
    let interactive = !plain && std::io::stdout().is_terminal();
    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let workers = spawn_workers(
        sorted_files,
        root,
        force,
        interactive,
        Resolver::new(client),
        tx,
        stop.clone(),
    );

    let counts = if interactive {
        match tag_ui::run(total, &rx, stop.clone()) {
            Ok(counts) => counts,
            Err(e) => {
                warn!("interactive view unavailable: {}", e);
                run_plain(total, rx)
            }
        }
    } else {
        run_plain(total, rx)
    };
    if !stop.load(Ordering::Relaxed) {
        for worker in workers {
            let _ = worker.join();
        }
    }

    println!(
        "  {} files tagged, {} unmatched, {} skipped, {} failed",
        console::style(counts.tagged).green(),
        console::style(counts.unmatched).yellow(),
        console::style(counts.skipped).dim(),
        console::style(counts.failed).red()
    );

    Ok(())
}

fn process_file(
    path: &Path,
    force: bool,
    resolver: &Resolver,
) -> Result<Outcome, Box<dyn std::error::Error>> {
    if !force && has_omm_tags(path) {
        return Ok(Outcome::Skipped(SkipReason::AlreadyTagged));
    }

    let Some(existing) = read_existing_metadata(path) else {
        debug!("no usable tags in {}", path.display());
        return Ok(Outcome::Skipped(SkipReason::NoTags));
    };
    let Some(title) = existing.title.clone().filter(|t| !t.trim().is_empty()) else {
        debug!("no title in {}", path.display());
        return Ok(Outcome::Skipped(SkipReason::NoTitle));
    };

    let artist = match non_empty(existing.artist.as_deref()) {
        Some(name) => resolver.resolve_artist(name)?,
        None => None,
    };
    let artist_name = artist
        .as_ref()
        .map(|a| a.attributes.name.as_str())
        .or(non_empty(existing.artist.as_deref()));
    let album = match non_empty(existing.album.as_deref()) {
        Some(name) => resolver.resolve_album(name, artist_name, artist.as_ref())?,
        None => None,
    };

    let artist_id = artist
        .as_ref()
        .map(|a| a.id.as_str())
        .filter(|id| valid_id(id, "artist"));
    let album_id = album
        .as_ref()
        .map(|a| a.id.as_str())
        .filter(|id| valid_id(id, "album"));

    let Some(song) = resolver.resolve_song(&existing, &title, album_id)? else {
        debug!("no match for {}", path.display());
        return Ok(Outcome::Skipped(SkipReason::NoMatch));
    };

    let mut candidate = song_to_candidate(song, album_id, artist_id, existing.album.as_deref());
    if let Some(ref artist) = existing.artist {
        prioritize_artist(&mut candidate.artists, artist);
        prioritize_artist(&mut candidate.album_artists, artist);
    }
    if let Some(name) = artist_name {
        prioritize_artist(&mut candidate.artists, name);
        prioritize_artist(&mut candidate.album_artists, name);
    }
    if candidate.album_artists.is_empty() {
        candidate.album_artists = candidate.artists.first().cloned().into_iter().collect();
    }
    candidate.album_artists.truncate(1);

    let report = apply_candidate(path, candidate, &existing, force, resolver)?;
    Ok(Outcome::Tagged(Box::new(report)))
}

pub(crate) fn apply_candidate(
    path: &Path,
    candidate: TagCandidate,
    existing: &ExistingMeta,
    force: bool,
    resolver: &Resolver,
) -> Result<Report, Box<dyn std::error::Error>> {
    let had_cover = has_front_cover(path);
    let cover = match candidate.artwork.as_deref() {
        Some(url) if force || !had_cover => resolver.cover_bytes(url),
        _ => None,
    };

    let embedded = write_metadata_tags(path, &candidate, cover.as_deref().map(Vec::as_slice))?;

    let mut report = report_for(existing, candidate);
    if embedded {
        if had_cover {
            report.changed.push("cover");
        } else {
            report.wrote.push("cover");
        }
    }
    Ok(report)
}

pub(crate) fn manual_candidate(
    song: OmmSong,
    artist: Option<&OmmArtist>,
    album: Option<&OmmAlbum>,
    existing: &ExistingMeta,
) -> TagCandidate {
    let artist_id = artist
        .map(|a| a.id.as_str())
        .filter(|id| valid_id(id, "artist"));
    let album_id = album
        .map(|a| a.id.as_str())
        .filter(|id| valid_id(id, "album"));
    let mut candidate = song_to_candidate(song, album_id, artist_id, existing.album.as_deref());

    if let Some(artist) = artist {
        prioritize_artist(&mut candidate.artists, &artist.attributes.name);
        prioritize_artist(&mut candidate.album_artists, &artist.attributes.name);
    }
    dedup_names(&mut candidate.artists);
    dedup_names(&mut candidate.album_artists);
    if candidate.album_artists.is_empty() {
        candidate.album_artists = candidate.artists.first().cloned().into_iter().collect();
    }
    candidate.album_artists.truncate(1);
    candidate
}

pub(crate) fn partial_candidate(
    title: &str,
    artist: Option<&OmmArtist>,
    album: Option<&OmmAlbum>,
    existing: &ExistingMeta,
) -> TagCandidate {
    let mut artists: Vec<String> = match artist {
        Some(artist) => vec![artist.attributes.name.clone()],
        None => existing.artist.iter().cloned().collect(),
    };
    dedup_names(&mut artists);

    let mut album_artists = match artist {
        Some(artist) => vec![artist.attributes.name.clone()],
        None => album
            .map(|a| a.attributes.artist_names.clone())
            .unwrap_or_default(),
    };
    dedup_names(&mut album_artists);
    album_artists.truncate(1);

    TagCandidate {
        title: title.to_string(),
        artists,
        album: album.map(|a| a.attributes.name.clone()),
        album_artists,
        date: album.and_then(|a| a.attributes.release_date.clone()),
        track: None,
        disc: None,
        genre: None,
        isrc: None,
        artwork: album.and_then(|a| a.attributes.artwork_url.clone()),
        song_id: String::new(),
        album_id: album.map(|a| a.id.clone()),
        artist_ids: artist.map(|a| a.id.clone()).into_iter().collect(),
    }
}

pub(crate) fn dedup_names(names: &mut Vec<String>) {
    let mut seen = HashSet::new();
    names.retain(|name| seen.insert(fold(name)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_ignore_accents_case_and_punctuation() {
        assert!(names_match("Celine Dion", "Céline Dion"));
        assert!(names_match("Da Höch Uf De Alpe", "Da höch uf de Alpe"));
        assert!(names_match("Dejeuner en Paix", "Déjeuner en paix"));
        assert!(names_match(
            "Menuet: Allegro Moderato",
            "Menuet- Allegro Moderato"
        ));
        assert!(!names_match("Daft Punk", "Queen"));
        assert!(!names_match("Li", "Lisa"));
    }

    #[test]
    fn partial_candidates_carry_no_song_id() {
        let artist = OmmArtist {
            id: "omm:artist:7".to_string(),
            attributes: OmmArtistAttributes {
                name: "Daft Punk".to_string(),
                artwork_url: None,
                genres: None,
            },
        };
        let candidate =
            partial_candidate("Aerodynamic", Some(&artist), None, &ExistingMeta::default());
        assert_eq!(candidate.artists, ["Daft Punk"]);
        assert_eq!(omm_ids(&candidate), [(ARTIST_ID_KEY, "7".to_string())]);
    }

    #[test]
    fn generic_artists_are_detected() {
        assert!(is_generic_artist("Various Artists"));
        assert!(is_generic_artist("Verschiedene Künstler"));
        assert!(!is_generic_artist("Daft Punk"));
    }
}
