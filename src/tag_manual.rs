use crate::tag::{
    Counts, Discovery, ExistingMeta, OmmAlbum, OmmArtist, OmmSong, Probe, Resolver, SongHint,
    apply_candidate, dedup_names, has_omm_tags, manual_candidate, names_match, partial_candidate,
    read_existing_metadata,
};
use crate::tag_ui::{
    ACCENT, FG, FG_DIM, FG_FAINT, FRAME, GREEN, RED, Restore, Term, blend, bold, clip,
    detect_background, fill, sp,
};

use crossterm::{
    event::{self, Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, enable_raw_mode},
};
use image::DynamicImage;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::{
    FilterType, Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const WORKERS: usize = 4;
const DEBOUNCE: Duration = Duration::from_millis(350);
const LABEL_WIDTH: usize = 10;
const MAX_COVER_ROWS: u16 = 10;
const MIN_COVER_ROWS: u16 = 4;
const DURATION_TOLERANCE_MS: u64 = 3000;
const DISCOVERY_SAMPLE: usize = 12;

#[derive(Clone, Copy, PartialEq)]
enum Stage {
    Artist,
    Album,
    Songs,
}

#[derive(Clone, Copy, PartialEq)]
enum MatchState {
    Idle,
    Pending,
    Done,
}

struct Slot {
    path: PathBuf,
    name: String,
    existing: ExistingMeta,
    title: String,
    variants: Vec<String>,
    song: Option<OmmSong>,
    state: MatchState,
}

struct Group {
    rel: String,
    has_artist: bool,
    has_album: bool,
    slots: Vec<Slot>,
    artist_query: String,
    album_query: String,
}

struct Input {
    text: Vec<char>,
    cursor: usize,
}

impl Input {
    fn new(value: &str) -> Self {
        let text: Vec<char> = value.chars().collect();
        Self {
            cursor: text.len(),
            text,
        }
    }

    fn value(&self) -> String {
        self.text.iter().collect()
    }

    fn handle(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
                true
            }
            KeyCode::Char('w') if ctrl => {
                let mut start = self.cursor;
                while start > 0 && self.text[start - 1] == ' ' {
                    start -= 1;
                }
                while start > 0 && self.text[start - 1] != ' ' {
                    start -= 1;
                }
                self.text.drain(start..self.cursor);
                self.cursor = start;
                true
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.text.insert(self.cursor, c);
                self.cursor += 1;
                true
            }
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.text.remove(self.cursor);
                true
            }
            KeyCode::Delete if self.cursor < self.text.len() => {
                self.text.remove(self.cursor);
                true
            }
            KeyCode::Left => {
                self.cursor = self.cursor.saturating_sub(1);
                false
            }
            KeyCode::Right => {
                self.cursor = (self.cursor + 1).min(self.text.len());
                false
            }
            KeyCode::Home => {
                self.cursor = 0;
                false
            }
            KeyCode::End => {
                self.cursor = self.text.len();
                false
            }
            _ => false,
        }
    }
}

struct Pane<T> {
    input: Input,
    results: Vec<T>,
    selected: usize,
    offset: usize,
    seq: u64,
    loading: bool,
    error: Option<String>,
    queried: Option<String>,
    edited: Option<Instant>,
    notes: Vec<String>,
    suggested: bool,
}

impl<T> Pane<T> {
    fn new(query: &str) -> Self {
        Self {
            input: Input::new(query),
            results: Vec::new(),
            selected: 0,
            offset: 0,
            seq: 0,
            loading: false,
            error: None,
            queried: None,
            edited: None,
            notes: Vec::new(),
            suggested: false,
        }
    }

    fn suggest(&mut self, results: Vec<T>, notes: Vec<String>) {
        self.queried = Some(self.query());
        self.results = results;
        self.notes = notes;
        self.selected = 0;
        self.offset = 0;
        self.loading = false;
        self.error = None;
        self.suggested = true;
    }

    fn query(&self) -> String {
        self.input.value().trim().to_string()
    }

    fn changed(&self) -> bool {
        self.queried.as_deref() != Some(self.query().as_str())
    }

    fn current(&self) -> Option<&T> {
        self.results.get(self.selected)
    }

    fn move_by(&mut self, delta: isize) {
        let last = self.results.len().saturating_sub(1);
        self.selected = self.selected.saturating_add_signed(delta).min(last);
    }

    fn due(&self) -> bool {
        self.changed()
            && !self.query().is_empty()
            && self.edited.is_some_and(|at| at.elapsed() >= DEBOUNCE)
    }

    fn apply(&mut self, result: Result<Vec<T>, String>) {
        self.loading = false;
        self.suggested = false;
        self.notes.clear();
        self.selected = 0;
        self.offset = 0;
        match result {
            Ok(results) => {
                self.results = results;
                self.error = None;
            }
            Err(e) => {
                self.results.clear();
                self.error = Some(e);
            }
        }
    }
}

enum PaneAction {
    None,
    Search,
    Select,
}

fn pane_key<T>(pane: &mut Pane<T>, key: &KeyEvent) -> PaneAction {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match key.code {
        KeyCode::Up => pane.move_by(-1),
        KeyCode::Down => pane.move_by(1),
        KeyCode::Char('p') if ctrl => pane.move_by(-1),
        KeyCode::Char('n') if ctrl => pane.move_by(1),
        KeyCode::PageUp => pane.move_by(-5),
        KeyCode::PageDown => pane.move_by(5),
        KeyCode::Enter => {
            return if pane.changed() || pane.results.is_empty() {
                PaneAction::Search
            } else {
                PaneAction::Select
            };
        }
        _ => {
            if pane.input.handle(key) {
                pane.edited = Some(Instant::now());
            }
        }
    }
    PaneAction::None
}

struct Pick {
    pane: Pane<OmmSong>,
    slot: usize,
    scoped: bool,
}

enum Thumb {
    Loading,
    Missing,
    Ready(Box<StatefulProtocol>),
}

enum Req {
    Artists {
        seq: u64,
        name: String,
    },
    Albums {
        seq: u64,
        name: String,
        artist_id: Option<String>,
    },
    Songs {
        seq: u64,
        name: String,
        hint: SongHint,
    },
    Match {
        gen_id: u64,
        slot: usize,
        variants: Vec<String>,
        hint: SongHint,
    },
    Discover {
        group_seq: u64,
        probes: Vec<Probe>,
    },
    Thumb(String),
}

enum Resp {
    Artists(u64, Result<Vec<OmmArtist>, String>),
    Albums(u64, Result<Vec<OmmAlbum>, String>),
    Songs(u64, Result<Vec<OmmSong>, String>),
    Matched {
        gen_id: u64,
        slot: usize,
        result: Result<Option<Box<OmmSong>>, String>,
    },
    Discovered {
        group_seq: u64,
        discovery: Discovery,
    },
    Thumb(String, Option<DynamicImage>),
}

fn spawn_workers(resolver: Arc<Resolver>) -> (Sender<Req>, Receiver<Resp>) {
    let (req_tx, req_rx) = mpsc::channel::<Req>();
    let (resp_tx, resp_rx) = mpsc::channel::<Resp>();
    let req_rx = Arc::new(Mutex::new(req_rx));

    for _ in 0..WORKERS {
        let (req_rx, resp_tx, resolver) = (req_rx.clone(), resp_tx.clone(), resolver.clone());
        thread::spawn(move || {
            loop {
                let req = match req_rx.lock().unwrap().recv() {
                    Ok(req) => req,
                    Err(_) => break,
                };
                let resp = match req {
                    Req::Artists { seq, name } => Resp::Artists(
                        seq,
                        resolver.search_artists(&name).map_err(|e| e.to_string()),
                    ),
                    Req::Albums {
                        seq,
                        name,
                        artist_id,
                    } => Resp::Albums(
                        seq,
                        resolver
                            .search_albums(&name, artist_id.as_deref())
                            .map_err(|e| e.to_string()),
                    ),
                    Req::Songs { seq, name, hint } => Resp::Songs(
                        seq,
                        resolver
                            .search_songs(&name, &hint)
                            .map_err(|e| e.to_string()),
                    ),
                    Req::Match {
                        gen_id,
                        slot,
                        variants,
                        hint,
                    } => Resp::Matched {
                        gen_id,
                        slot,
                        result: resolver
                            .match_song(&variants, &hint)
                            .map(|song| song.map(Box::new))
                            .map_err(|e| e.to_string()),
                    },
                    Req::Discover { group_seq, probes } => Resp::Discovered {
                        group_seq,
                        discovery: resolver.discover(&probes),
                    },
                    Req::Thumb(url) => {
                        let image = resolver.preview(&url);
                        Resp::Thumb(url, image)
                    }
                };
                if resp_tx.send(resp).is_err() {
                    break;
                }
            }
        });
    }

    (req_tx, resp_rx)
}

struct App {
    groups: Vec<Group>,
    gi: usize,
    stage: Stage,
    artist: Pane<OmmArtist>,
    album: Pane<OmmAlbum>,
    chosen_artist: Option<OmmArtist>,
    chosen_album: Option<OmmAlbum>,
    album_scope: Option<String>,
    matched_for: Option<(Option<String>, Option<String>)>,
    discovery: Option<Discovery>,
    discovering: bool,
    group_seq: u64,
    slots: Vec<Slot>,
    cursor: usize,
    offset: usize,
    pick: Option<Pick>,
    gen_id: u64,
    seq: u64,
    thumbs: HashMap<String, Thumb>,
    tx: Sender<Req>,
    resolver: Arc<Resolver>,
    force: bool,
    counts: Counts,
    status: Option<(String, bool)>,
    picker: Picker,
    background: Option<(u8, u8, u8)>,
    quit: bool,
}

fn mode<'a>(values: impl Iterator<Item = Option<&'a str>>) -> Option<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for value in values.flatten().map(str::trim).filter(|v| !v.is_empty()) {
        *counts.entry(value).or_default() += 1;
    }
    counts
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(value, _)| value.to_string())
}

fn dir_name(path: Option<&Path>) -> String {
    path.and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn build_groups(files: Vec<PathBuf>, force: bool) -> Vec<Group> {
    let slots: Vec<(PathBuf, Slot)> = files
        .par_iter()
        .filter(|path| force || !has_omm_tags(path))
        .map(|path| {
            let existing = read_existing_metadata(path).unwrap_or_default();
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = path
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            let (title, variants) = title_variants(existing.title.as_deref(), &stem);
            let dir = path.parent().unwrap_or(Path::new("")).to_path_buf();
            (
                dir,
                Slot {
                    path: path.clone(),
                    name,
                    existing,
                    title,
                    variants,
                    song: None,
                    state: MatchState::Idle,
                },
            )
        })
        .collect();

    let mut by_dir: BTreeMap<PathBuf, Vec<Slot>> = BTreeMap::new();
    for (dir, slot) in slots {
        by_dir.entry(dir).or_default().push(slot);
    }

    by_dir
        .into_iter()
        .map(|(dir, mut slots)| {
            slots.sort_by(|a, b| a.path.cmp(&b.path));
            let album_dir = dir_name(Some(&dir));
            let artist_dir = dir_name(dir.parent());
            let artist_tag = mode(slots.iter().map(|s| s.existing.artist.as_deref()));
            let album_tag = mode(slots.iter().map(|s| s.existing.album.as_deref()));
            let (has_artist, has_album) = (artist_tag.is_some(), album_tag.is_some());
            let artist_query = artist_tag.unwrap_or(artist_dir.clone());
            let album_query = album_tag.unwrap_or(album_dir.clone());
            let rel = if artist_dir.is_empty() {
                album_dir
            } else {
                format!("{}/{}", artist_dir, album_dir)
            };
            Group {
                rel,
                has_artist,
                has_album,
                slots,
                artist_query,
                album_query,
            }
        })
        .collect()
}

fn strip_track_number(text: &str) -> &str {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    if !(1..=3).contains(&digits) {
        return text;
    }
    let Some(rest) = text[digits..]
        .trim_start()
        .strip_prefix(['.', '-', '_', ')'])
    else {
        return text;
    };
    let rest = rest.trim_start_matches([' ', '.', '-', '_', ')']);
    if rest.is_empty() { text } else { rest }
}

fn strip_leading_number(text: &str) -> &str {
    let digits = text.bytes().take_while(u8::is_ascii_digit).count();
    match text[digits..].strip_prefix(' ') {
        Some(rest) if (1..=3).contains(&digits) && !rest.trim().is_empty() => rest.trim_start(),
        _ => text,
    }
}

fn title_variants(tag_title: Option<&str>, stem: &str) -> (String, Vec<String>) {
    let tagged = tag_title.map(str::trim).filter(|t| !t.is_empty());
    let base = tagged.unwrap_or(stem.trim());
    let stripped = strip_track_number(base).trim();
    let spaced = strip_leading_number(stripped).trim();
    let dashed = stripped
        .rsplit_once(" - ")
        .map(|(_, title)| title.trim())
        .unwrap_or(stripped);

    let mut variants: Vec<String> = Vec::new();
    for candidate in [base, stripped, spaced, dashed] {
        if !candidate.is_empty() && !variants.iter().any(|v| v == candidate) {
            variants.push(candidate.to_string());
        }
    }
    let title = if tagged.is_some() { base } else { stripped };
    (title.to_string(), variants)
}

fn year(date: &Option<String>) -> Option<String> {
    date.as_deref()
        .map(|d| d.chars().take(4).collect::<String>())
        .filter(|y| !y.is_empty())
}

fn joined(names: &[String]) -> String {
    let mut names = names.to_vec();
    dedup_names(&mut names);
    names.join(", ")
}

fn clock(ms: u64) -> String {
    let secs = ms / 1000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

impl App {
    fn new(
        groups: Vec<Group>,
        tx: Sender<Req>,
        resolver: Arc<Resolver>,
        force: bool,
        picker: Picker,
        background: Option<(u8, u8, u8)>,
    ) -> Self {
        let mut app = Self {
            groups,
            gi: 0,
            stage: Stage::Artist,
            artist: Pane::new(""),
            album: Pane::new(""),
            chosen_artist: None,
            chosen_album: None,
            album_scope: None,
            matched_for: None,
            discovery: None,
            discovering: false,
            group_seq: 0,
            slots: Vec::new(),
            cursor: 0,
            offset: 0,
            pick: None,
            gen_id: 0,
            seq: 0,
            thumbs: HashMap::new(),
            tx,
            resolver,
            force,
            counts: Counts::default(),
            status: None,
            picker,
            background,
            quit: false,
        };
        app.load_group();
        app
    }

    fn panel(&self) -> Color {
        self.background.map_or(Color::Reset, |bg| blend(bg, 5))
    }

    fn bump(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    fn load_group(&mut self) {
        let group = &mut self.groups[self.gi];
        self.slots = std::mem::take(&mut group.slots);
        self.artist = Pane::new(&group.artist_query);
        self.album = Pane::new(&group.album_query);
        let has_artist = group.has_artist;
        let group_artist = has_artist.then(|| group.artist_query.clone());
        self.chosen_artist = None;
        self.chosen_album = None;
        self.album_scope = None;
        self.matched_for = None;
        self.cursor = 0;
        self.offset = 0;
        self.pick = None;
        self.gen_id += 1;
        self.stage = Stage::Artist;
        self.discovery = None;
        self.discovering = true;
        self.group_seq += 1;

        let step = self.slots.len().div_ceil(DISCOVERY_SAMPLE).max(1);
        let probes: Vec<Probe> = self
            .slots
            .iter()
            .step_by(step)
            .map(|slot| Probe {
                variants: slot.variants.clone(),
                hint: SongHint {
                    artist: slot.existing.artist.clone().or(group_artist.clone()),
                    album: slot.existing.album.clone(),
                    duration_ms: slot.existing.duration_ms,
                    ..SongHint::default()
                },
            })
            .collect();
        let _ = self.tx.send(Req::Discover {
            group_seq: self.group_seq,
            probes,
        });

        if has_artist {
            self.search_artists();
        } else {
            self.artist.loading = true;
            self.artist.queried = Some(self.artist.query());
        }
    }

    fn next_group(&mut self) {
        self.gi += 1;
        if self.gi >= self.groups.len() {
            self.quit = true;
        } else {
            self.load_group();
        }
    }

    fn search_artists(&mut self) {
        let name = self.artist.query();
        if name.is_empty() {
            return;
        }
        let seq = self.bump();
        self.artist.seq = seq;
        self.artist.loading = true;
        self.artist.error = None;
        self.artist.queried = Some(name.clone());
        let _ = self.tx.send(Req::Artists { seq, name });
    }

    fn search_albums(&mut self) {
        let name = self.album.query();
        if name.is_empty() {
            return;
        }
        let artist_id = self.chosen_artist.as_ref().map(|a| a.id.clone());
        let seq = self.bump();
        self.album.seq = seq;
        self.album.loading = true;
        self.album.error = None;
        self.album.queried = Some(name.clone());
        self.album_scope = artist_id.clone();
        let _ = self.tx.send(Req::Albums {
            seq,
            name,
            artist_id,
        });
    }

    fn search_pick(&mut self) {
        let artist_id = self.chosen_artist.as_ref().map(|a| a.id.clone());
        let album_id = self.chosen_album.as_ref().map(|a| a.id.clone());
        let seq = self.bump();
        let Some(pick) = self.pick.as_mut() else {
            return;
        };
        let name = pick.pane.query();
        if name.is_empty() {
            return;
        }
        pick.pane.seq = seq;
        pick.pane.loading = true;
        pick.pane.error = None;
        pick.pane.queried = Some(name.clone());
        let hint = if pick.scoped {
            SongHint {
                artist_id,
                album_id,
                ..SongHint::default()
            }
        } else {
            SongHint::default()
        };
        let _ = self.tx.send(Req::Songs { seq, name, hint });
    }

    fn suggest_artists(&mut self) -> bool {
        let Some(discovery) = self.discovery.as_ref() else {
            return false;
        };
        if discovery.artists.is_empty() {
            return false;
        }
        let (items, notes): (Vec<_>, Vec<_>) = discovery
            .artists
            .iter()
            .map(|s| (s.item.clone(), format!("{}/{} files", s.votes, s.total)))
            .unzip();
        self.artist.suggest(items, notes);
        true
    }

    fn suggest_albums(&mut self) -> bool {
        let Some(discovery) = self.discovery.as_ref() else {
            return false;
        };
        let artist_name = self
            .chosen_artist
            .as_ref()
            .map(|a| a.attributes.name.clone());
        let by_artist: Vec<_> = discovery
            .albums
            .iter()
            .filter(|s| {
                artist_name.as_deref().is_none_or(|name| {
                    s.item
                        .attributes
                        .artist_names
                        .iter()
                        .any(|a| names_match(a, name))
                })
            })
            .collect();
        let list = if by_artist.is_empty() {
            discovery.albums.iter().collect()
        } else {
            by_artist
        };
        if list.is_empty() {
            return false;
        }
        let (items, notes): (Vec<_>, Vec<_>) = list
            .into_iter()
            .map(|s| (s.item.clone(), format!("{}/{} files", s.votes, s.total)))
            .unzip();
        self.album.suggest(items, notes);
        self.album_scope = self.chosen_artist.as_ref().map(|a| a.id.clone());
        true
    }

    fn enter_album(&mut self) {
        self.stage = Stage::Album;
        let scope = self.chosen_artist.as_ref().map(|a| a.id.clone());
        let stale =
            self.album.results.is_empty() || self.album.changed() || self.album_scope != scope;
        if !stale {
            return;
        }

        if !self.groups[self.gi].has_album && self.album.edited.is_none() {
            if self.discovering {
                self.album.loading = true;
                self.album.queried = Some(self.album.query());
                self.album_scope = scope;
                return;
            }
            if self.suggest_albums() {
                return;
            }
        }
        self.search_albums();
    }

    fn enter_songs(&mut self) {
        self.stage = Stage::Songs;
        let key = (
            self.chosen_artist.as_ref().map(|a| a.id.clone()),
            self.chosen_album.as_ref().map(|a| a.id.clone()),
        );
        if self.matched_for.as_ref() != Some(&key) {
            self.matched_for = Some(key);
            for slot in &mut self.slots {
                slot.song = None;
            }
            self.rematch();
        }
        self.goto(self.cursor);
    }

    fn rematch(&mut self) {
        self.gen_id += 1;
        let artist_id = self.chosen_artist.as_ref().map(|a| a.id.clone());
        let album_id = self.chosen_album.as_ref().map(|a| a.id.clone());
        let artist = self
            .chosen_artist
            .as_ref()
            .map(|a| a.attributes.name.clone());
        let album = self
            .chosen_album
            .as_ref()
            .map(|a| a.attributes.name.clone());
        for (index, slot) in self.slots.iter_mut().enumerate() {
            if slot.song.is_some() {
                continue;
            }
            slot.state = MatchState::Pending;
            let _ = self.tx.send(Req::Match {
                gen_id: self.gen_id,
                slot: index,
                variants: slot.variants.clone(),
                hint: SongHint {
                    artist_id: artist_id.clone(),
                    album_id: album_id.clone(),
                    artist: artist.clone().or(slot.existing.artist.clone()),
                    album: album.clone().or(slot.existing.album.clone()),
                    duration_ms: slot.existing.duration_ms,
                },
            });
        }
    }

    fn invalidate_album(&mut self) {
        self.chosen_album = None;
        self.album.results.clear();
        self.album.queried = None;
    }

    fn select_artist(&mut self) {
        let Some(artist) = self.artist.current().cloned() else {
            return;
        };
        let changed = self.chosen_artist.as_ref().map(|a| &a.id) != Some(&artist.id);
        self.chosen_artist = Some(artist);
        if changed {
            self.invalidate_album();
        }
        self.enter_album();
    }

    fn forward_artist(&mut self) {
        self.enter_album();
    }

    fn select_album(&mut self) {
        let Some(album) = self.album.current().cloned() else {
            return;
        };
        self.chosen_album = Some(album);
        self.enter_songs();
    }

    fn forward_album(&mut self) {
        self.enter_songs();
    }

    fn back(&mut self) {
        match self.stage {
            Stage::Artist => {}
            Stage::Album => self.stage = Stage::Artist,
            Stage::Songs => {
                self.pick = None;
                self.stage = Stage::Album;
            }
        }
    }

    fn goto(&mut self, index: usize) {
        self.cursor = index.min(self.slots.len() + 1);
        self.pick = None;
        if self.cursor < self.slots.len() {
            self.open_pick();
        }
    }

    fn open_pick(&mut self) {
        let Some(slot) = self.slots.get(self.cursor) else {
            return;
        };
        self.pick = Some(Pick {
            pane: Pane::new(&slot.title),
            slot: self.cursor,
            scoped: self.chosen_album.is_some() || self.chosen_artist.is_some(),
        });
        self.search_pick();
    }

    fn select_pick(&mut self) {
        let Some(pick) = self.pick.as_ref() else {
            return;
        };
        let index = pick.slot;
        let song = if pick.pane.query().is_empty() {
            None
        } else {
            let Some(song) = pick.pane.current().cloned() else {
                return;
            };
            Some(song)
        };
        if let Some(slot) = self.slots.get_mut(index) {
            slot.song = song;
            slot.state = MatchState::Done;
        }
        self.goto(index + 1);
    }

    fn skip_folder(&mut self) {
        self.counts.skipped += self.slots.len();
        self.status = Some((format!("skipped {}", self.groups[self.gi].rel), false));
        self.next_group();
    }

    fn chosen_any(&self) -> bool {
        self.chosen_artist.is_some() || self.chosen_album.is_some()
    }

    fn writable(&self) -> usize {
        if self.chosen_any() {
            self.slots.len()
        } else {
            self.slots.iter().filter(|s| s.song.is_some()).count()
        }
    }

    fn save(&mut self) {
        if self.writable() == 0 {
            self.status = Some(("pick an artist or album first".to_string(), false));
            return;
        }
        self.write_group();
    }

    fn write_group(&mut self) {
        let (mut tagged, mut partial, mut failed, mut left) = (0, 0, 0, 0);
        let mut first_error = None;
        let chosen_any = self.chosen_any();
        for slot in &self.slots {
            let candidate = match slot.song.clone() {
                Some(song) => manual_candidate(
                    song,
                    self.chosen_artist.as_ref(),
                    self.chosen_album.as_ref(),
                    &slot.existing,
                ),
                None if chosen_any => partial_candidate(
                    &slot.title,
                    self.chosen_artist.as_ref(),
                    self.chosen_album.as_ref(),
                    &slot.existing,
                ),
                None => {
                    left += 1;
                    continue;
                }
            };
            let is_partial = slot.song.is_none();
            match apply_candidate(
                &slot.path,
                candidate,
                &slot.existing,
                self.force,
                &self.resolver,
            ) {
                Ok(_) => {
                    tagged += 1;
                    partial += usize::from(is_partial);
                }
                Err(e) => {
                    failed += 1;
                    first_error.get_or_insert(format!("{}: {}", slot.name, e));
                }
            }
        }
        self.counts.tagged += tagged;
        self.counts.skipped += left;
        self.counts.failed += failed;
        self.status = Some(match first_error {
            Some(e) => (format!("{} failed, {}", failed, e), false),
            None if partial > 0 => (
                format!("wrote {} files, {} with artist/album only", tagged, partial),
                true,
            ),
            None => (format!("wrote {} files", tagged), true),
        });
        self.next_group();
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('s') {
            self.save();
            return;
        }
        if key.code == KeyCode::BackTab {
            self.back();
            return;
        }

        match self.stage {
            Stage::Artist => match key.code {
                KeyCode::Tab => self.forward_artist(),
                _ => match pane_key(&mut self.artist, &key) {
                    PaneAction::Search => self.search_artists(),
                    PaneAction::Select => self.select_artist(),
                    PaneAction::None => {}
                },
            },
            Stage::Album => match key.code {
                KeyCode::Esc => self.stage = Stage::Artist,
                KeyCode::Tab => self.forward_album(),
                _ => match pane_key(&mut self.album, &key) {
                    PaneAction::Search => self.search_albums(),
                    PaneAction::Select => self.select_album(),
                    PaneAction::None => {}
                },
            },
            Stage::Songs => self.on_songs_key(key),
        }
    }

    fn on_songs_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.back(),
            KeyCode::PageUp => self.goto(self.cursor.saturating_sub(1)),
            KeyCode::PageDown => self.goto(self.cursor + 1),
            _ if self.pick.is_some() => {
                if key.code == KeyCode::Tab {
                    if let Some(pick) = self.pick.as_mut() {
                        pick.scoped = !pick.scoped;
                    }
                    self.search_pick();
                    return;
                }
                let Some(pick) = self.pick.as_mut() else {
                    return;
                };
                match pane_key(&mut pick.pane, &key) {
                    PaneAction::Search => self.search_pick(),
                    PaneAction::Select => self.select_pick(),
                    PaneAction::None => {}
                }
            }
            KeyCode::Up => self.goto(self.cursor.saturating_sub(1)),
            KeyCode::Down => self.goto(self.cursor + 1),
            KeyCode::Enter => {
                if self.cursor == self.slots.len() {
                    self.save();
                } else {
                    self.skip_folder();
                }
            }
            _ => {}
        }
    }

    fn on_resp(&mut self, resp: Resp) {
        match resp {
            Resp::Artists(seq, result) if seq == self.artist.seq => self.artist.apply(result),
            Resp::Albums(seq, result) if seq == self.album.seq => self.album.apply(result),
            Resp::Songs(seq, result) => {
                if let Some(pick) = self.pick.as_mut()
                    && pick.pane.seq == seq
                {
                    pick.pane.apply(result);
                    let current = self
                        .slots
                        .get(pick.slot)
                        .and_then(|slot| slot.song.as_ref())
                        .map(|song| song.id.as_str());
                    if let Some(index) =
                        current.and_then(|id| pick.pane.results.iter().position(|s| s.id == id))
                    {
                        pick.pane.selected = index;
                    }
                }
            }
            Resp::Matched {
                gen_id,
                slot,
                result,
            } if gen_id == self.gen_id => {
                if let Some(slot) = self.slots.get_mut(slot) {
                    slot.state = MatchState::Done;
                    if let Ok(Some(song)) = result
                        && slot.song.is_none()
                    {
                        slot.song = Some(*song);
                    }
                }
            }
            Resp::Discovered {
                group_seq,
                discovery,
            } if group_seq == self.group_seq => {
                self.discovering = false;
                self.discovery = Some(discovery);
                let (has_artist, has_album) = {
                    let group = &self.groups[self.gi];
                    (group.has_artist, group.has_album)
                };
                if !has_artist && self.artist.edited.is_none() && self.artist.results.is_empty() {
                    self.artist.loading = false;
                    if !self.suggest_artists() {
                        self.search_artists();
                    }
                }
                if !has_album
                    && self.stage != Stage::Artist
                    && self.album.edited.is_none()
                    && self.album.results.is_empty()
                {
                    self.album.loading = false;
                    if !self.suggest_albums() {
                        self.search_albums();
                    }
                }
            }
            Resp::Thumb(url, image) => {
                let thumb = match image {
                    Some(image) => Thumb::Ready(Box::new(self.picker.new_resize_protocol(image))),
                    None => Thumb::Missing,
                };
                self.thumbs.insert(url, thumb);
            }
            _ => {}
        }
    }

    fn tick(&mut self) {
        if let Some(pick) = self.pick.as_ref() {
            if pick.pane.due() {
                self.search_pick();
            }
        } else {
            match self.stage {
                Stage::Artist if self.artist.due() => self.search_artists(),
                Stage::Album if self.album.due() => self.search_albums(),
                Stage::Artist if self.artist.query().is_empty() && !self.artist.suggested => {
                    self.suggest_artists();
                }
                Stage::Album if self.album.query().is_empty() && !self.album.suggested => {
                    self.suggest_albums();
                }
                _ => {}
            }
        }

        if let Some(url) = self.focused_artwork()
            && !self.thumbs.contains_key(&url)
        {
            self.thumbs.insert(url.clone(), Thumb::Loading);
            let _ = self.tx.send(Req::Thumb(url));
        }
    }

    fn focused_song(&self) -> Option<&OmmSong> {
        match &self.pick {
            Some(pick) => pick.pane.current(),
            None => self.slots.get(self.cursor).and_then(|s| s.song.as_ref()),
        }
    }

    fn focused_artwork(&self) -> Option<String> {
        match self.stage {
            Stage::Artist => self.artist.current()?.attributes.artwork_url.clone(),
            Stage::Album => self.album.current()?.attributes.artwork_url.clone(),
            Stage::Songs => self.focused_song()?.attributes.artwork_url.clone(),
        }
    }
}

fn field(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![sp(format!("{:<w$}", label, w = LABEL_WIDTH), FG_DIM)];
    spans.extend(value);
    Line::from(spans)
}

fn artist_lines(artist: &OmmArtist, room: usize) -> Vec<Line<'static>> {
    let value = room.saturating_sub(LABEL_WIDTH);
    let mut lines = vec![field(
        "name",
        vec![bold(clip(&artist.attributes.name, value), FG)],
    )];
    if let Some(genres) = artist.attributes.genres.as_ref().filter(|g| !g.is_empty()) {
        lines.push(field(
            "genres",
            vec![sp(clip(&genres.join(", "), value), FG)],
        ));
    }
    lines.push(field("id", vec![sp(clip(&artist.id, value), FG_FAINT)]));
    lines
}

fn album_lines(album: &OmmAlbum, room: usize) -> Vec<Line<'static>> {
    let value = room.saturating_sub(LABEL_WIDTH);
    let attrs = &album.attributes;
    let mut lines = vec![
        field("album", vec![bold(clip(&attrs.name, value), FG)]),
        field(
            "artist",
            vec![sp(clip(&joined(&attrs.artist_names), value), FG)],
        ),
    ];
    if let Some(date) = attrs.release_date.as_ref() {
        lines.push(field("released", vec![sp(date.clone(), FG)]));
    }
    if let Some(count) = attrs.track_count {
        lines.push(field("tracks", vec![sp(count.to_string(), FG)]));
    }
    lines.push(field("id", vec![sp(clip(&album.id, value), FG_FAINT)]));
    lines
}

fn song_lines(song: &OmmSong, local_ms: Option<u128>, room: usize) -> Vec<Line<'static>> {
    let value = room.saturating_sub(LABEL_WIDTH);
    let attrs = &song.attributes;
    let mut lines = vec![
        field("title", vec![bold(clip(&attrs.name, value), FG)]),
        field(
            "artist",
            vec![sp(clip(&joined(&attrs.artist_names), value), FG)],
        ),
    ];
    if let Some(album) = attrs.album_name.as_ref() {
        lines.push(field("album", vec![sp(clip(album, value), FG)]));
    }

    let mut position = Vec::new();
    if let Some(track) = attrs.track_number {
        position.push(format!("track {}", track));
    }
    if let Some(disc) = attrs.disc_number {
        position.push(format!("disc {}", disc));
    }
    if !position.is_empty() {
        lines.push(field("position", vec![sp(position.join("  ·  "), FG)]));
    }
    if let Some(date) = attrs.release_date.as_ref() {
        lines.push(field("released", vec![sp(date.clone(), FG)]));
    }
    if let Some(ms) = attrs.duration_ms {
        let mut spans = vec![sp(clock(ms), FG)];
        if let Some(local) = local_ms {
            let local = local as u64;
            let color = if local.abs_diff(ms) > DURATION_TOLERANCE_MS {
                ACCENT
            } else {
                FG_DIM
            };
            spans.push(sp(format!("   local {}", clock(local)), color));
        }
        lines.push(field("length", spans));
    }
    if let Some(genres) = attrs.genres.as_ref().filter(|g| !g.is_empty()) {
        lines.push(field(
            "genre",
            vec![sp(clip(&genres.join("; "), value), FG)],
        ));
    }
    if let Some(isrc) = attrs.isrc.as_ref() {
        lines.push(field("isrc", vec![sp(isrc.clone(), FG_DIM)]));
    }
    lines.push(field("id", vec![sp(clip(&song.id, value), FG_FAINT)]));
    lines
}

fn input_spans(input: &Input, room: usize) -> Vec<Span<'static>> {
    let room = room.max(2);
    let start = (input.cursor + 1).saturating_sub(room);
    let visible: Vec<char> = input.text.iter().skip(start).take(room).copied().collect();
    let at = (input.cursor - start).min(visible.len());
    let before: String = visible[..at].iter().collect();
    let under = visible.get(at).copied().unwrap_or(' ');
    let after: String = visible.iter().skip(at + 1).collect();
    vec![
        sp(before, FG),
        Span::styled(
            under.to_string(),
            Style::default().fg(FG).add_modifier(Modifier::REVERSED),
        ),
        sp(after, FG),
    ]
}

fn draw_search<T>(
    f: &mut Frame,
    panel: Color,
    area: Rect,
    title: &str,
    pane: &mut Pane<T>,
    row: impl Fn(&T, &str, usize) -> Vec<Span<'static>>,
    field: Color,
) {
    fill(f, area, panel);
    let inner = area.inner(Margin::new(2, 1));
    if inner.height < 5 || inner.width < 10 {
        return;
    }

    let line = |offset: u16| Rect {
        y: inner.y + offset,
        height: 1,
        ..inner
    };

    f.render_widget(Paragraph::new(Line::from(sp(title, FG_FAINT))), line(0));

    let mut prompt = vec![sp("› ", ACCENT)];
    prompt.extend(input_spans(&pane.input, inner.width as usize - 2));
    fill(f, line(1), field);
    f.render_widget(
        Paragraph::new(Line::from(prompt)).style(Style::default().bg(field)),
        line(1),
    );

    let status = if pane.loading {
        sp("searching…", FG_DIM)
    } else if let Some(error) = pane.error.as_ref() {
        sp(clip(error, inner.width as usize), RED)
    } else if pane.suggested {
        sp("suggested from your files", GREEN)
    } else if pane.changed() {
        sp("searching in a moment…", FG_FAINT)
    } else if pane.results.is_empty() {
        sp("no results", ACCENT)
    } else {
        sp(format!("{} results", pane.results.len()), FG_FAINT)
    };
    f.render_widget(Paragraph::new(Line::from(status)), line(2));

    let rows = (inner.height - 4) as usize;
    if pane.selected < pane.offset {
        pane.offset = pane.selected;
    } else if pane.selected >= pane.offset + rows {
        pane.offset = pane.selected + 1 - rows;
    }

    let room = inner.width as usize - 2;
    let lines: Vec<Line> = pane
        .results
        .iter()
        .enumerate()
        .skip(pane.offset)
        .take(rows)
        .map(|(index, item)| {
            let marker = if index == pane.selected {
                bold("▸ ", ACCENT)
            } else {
                sp("  ", FG)
            };
            let note = pane.notes.get(index).map_or("", String::as_str);
            let mut spans = vec![marker];
            spans.extend(row(item, note, room));
            Line::from(spans)
        })
        .collect();
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            y: inner.y + 4,
            height: inner.height - 4,
            ..inner
        },
    );
}

fn with_extra(main: &str, extra: &[String], room: usize) -> Vec<Span<'static>> {
    let main = clip(main, room);
    let used = main.chars().count();
    let extra = extra
        .iter()
        .filter(|e| !e.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(" · ");
    let mut spans = vec![sp(main, FG)];
    if used + 6 < room && !extra.is_empty() {
        spans.push(sp(format!("  {}", clip(&extra, room - used - 2)), FG_FAINT));
    }
    spans
}

fn artist_row(artist: &OmmArtist, note: &str, room: usize) -> Vec<Span<'static>> {
    let genres = artist
        .attributes
        .genres
        .as_ref()
        .map(|g| g.join(", "))
        .unwrap_or_default();
    with_extra(&artist.attributes.name, &[note.to_string(), genres], room)
}

fn album_row(album: &OmmAlbum, note: &str, room: usize) -> Vec<Span<'static>> {
    let attrs = &album.attributes;
    let extra = [
        note.to_string(),
        joined(&attrs.artist_names),
        year(&attrs.release_date).unwrap_or_default(),
        attrs
            .track_count
            .map(|c| format!("{} tracks", c))
            .unwrap_or_default(),
    ];
    with_extra(&attrs.name, &extra, room)
}

fn song_row(song: &OmmSong, _note: &str, room: usize) -> Vec<Span<'static>> {
    let attrs = &song.attributes;
    let extra = [
        joined(&attrs.artist_names),
        attrs.album_name.clone().unwrap_or_default(),
        attrs
            .track_number
            .map(|t| format!("#{}", t))
            .unwrap_or_default(),
    ];
    with_extra(&attrs.name, &extra, room)
}

fn cell(text: &str, width: usize) -> String {
    format!("{:<w$}", clip(text, width), w = width)
}

fn draw_summary(f: &mut Frame, app: &App, area: Rect) {
    fill(f, area, app.panel());
    let inner = area.inner(Margin::new(2, 1));
    let matched = app.slots.iter().filter(|s| s.song.is_some()).count();
    let lines = if app.cursor == app.slots.len() {
        vec![
            Line::from(bold(
                format!("write {} of {} files", app.writable(), app.slots.len()),
                GREEN,
            )),
            Line::from(""),
            Line::from(sp(
                format!(
                    "{} matched to a song, the rest get only the chosen artist/album",
                    matched
                ),
                FG_DIM,
            )),
            Line::from(sp("enter or ctrl-s writes this folder", FG_DIM)),
            Line::from(sp("page up goes back to review files", FG_FAINT)),
        ]
    } else {
        vec![
            Line::from(bold("skip this folder", FG)),
            Line::from(""),
            Line::from(sp("enter leaves these files untouched", FG_DIM)),
        ]
    };
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_table(f: &mut Frame, app: &mut App, area: Rect) {
    let panel = app.panel();
    fill(f, area, panel);
    let inner = area.inner(Margin::new(2, 1));
    if inner.height < 4 || inner.width < 24 {
        return;
    }

    let matched = app.slots.iter().filter(|s| s.song.is_some()).count();
    let pending = app
        .slots
        .iter()
        .filter(|s| s.state == MatchState::Pending)
        .count();

    let room = inner.width as usize - 2;
    let left = (room - 3) / 2;
    let right = room - 3 - left;
    let lengths = room >= 70;

    let remote_heading = if pending > 0 {
        format!("omm match · matching {}…", pending)
    } else {
        format!("omm match · {} of {}", matched, app.slots.len())
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            sp("  ", FG),
            sp(cell("your files", left), FG_FAINT),
            sp("   ", FG),
            sp(cell(&remote_heading, right), FG_FAINT),
        ])),
        Rect { height: 1, ..inner },
    );

    let mut seen: HashMap<&str, usize> = HashMap::new();
    for slot in &app.slots {
        if let Some(song) = slot.song.as_ref() {
            *seen.entry(song.id.as_str()).or_default() += 1;
        }
    }

    let total = app.slots.len() + 2;
    let rows = (inner.height - 2) as usize;
    if app.cursor < app.offset {
        app.offset = app.cursor;
    } else if app.cursor >= app.offset + rows {
        app.offset = app.cursor + 1 - rows;
    }

    let mut lines: Vec<Line> = Vec::new();
    for index in app.offset..total.min(app.offset + rows) {
        let selected = index == app.cursor;
        let marker = if selected {
            bold("▸ ", ACCENT)
        } else {
            sp("  ", FG)
        };

        if index >= app.slots.len() {
            let write = index == app.slots.len();
            let label = if write {
                format!("write {} files", app.writable())
            } else {
                "skip this folder".to_string()
            };
            let color = match (write, app.writable() > 0, selected) {
                (true, true, _) => GREEN,
                (_, _, true) => FG,
                _ => FG_DIM,
            };
            lines.push(Line::from(vec![
                marker,
                if selected {
                    bold(label, color)
                } else {
                    sp(label, color)
                },
            ]));
            continue;
        }

        let slot = &app.slots[index];
        let local_ms = slot.existing.duration_ms.map(|ms| ms as u64);
        let local_text = if lengths {
            let length = local_ms.map(clock).unwrap_or_default();
            format!("{} {:>5}", cell(&slot.title, left - 6), length)
        } else {
            cell(&slot.title, left)
        };
        let local = if selected {
            bold(local_text, FG)
        } else {
            sp(local_text, FG_DIM)
        };

        let mut spans = vec![marker, local, sp(" │ ", FG_FAINT)];
        match (slot.song.as_ref(), slot.state) {
            (Some(song), _) => {
                let attrs = &song.attributes;
                let title = match attrs.track_number {
                    Some(track) => format!("{:>2} {}", track, attrs.name),
                    None => attrs.name.clone(),
                };
                let duplicate = seen.get(song.id.as_str()).copied().unwrap_or(0) > 1;
                let color = if duplicate { RED } else { GREEN };
                if lengths {
                    spans.push(sp(cell(&title, right - 7), color));
                    let remote = attrs.duration_ms;
                    spans.push(sp(
                        format!(" {:>5}", remote.map(clock).unwrap_or_default()),
                        FG_DIM,
                    ));
                    let off = matches!(
                        (local_ms, remote),
                        (Some(l), Some(r)) if l.abs_diff(r) > DURATION_TOLERANCE_MS
                    );
                    spans.push(if off { sp("≠", ACCENT) } else { sp(" ", FG) });
                } else {
                    spans.push(sp(cell(&title, right), color));
                }
            }
            (None, MatchState::Pending) => spans.push(sp("…", FG_FAINT)),
            (None, MatchState::Done) => spans.push(sp("no match", ACCENT)),
            (None, MatchState::Idle) => {}
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(
        Paragraph::new(lines),
        Rect {
            y: inner.y + 2,
            height: inner.height - 2,
            ..inner
        },
    );
}

fn focused_slot(app: &App) -> Option<&Slot> {
    let index = app.pick.as_ref().map_or(app.cursor, |p| p.slot);
    app.slots.get(index)
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let panel = app.panel();
    fill(f, area, panel);
    let inner = area.inner(Margin::new(2, 1));
    if inner.height < 3 || inner.width < 16 {
        return;
    }

    let room = inner.width as usize;
    let (title, lines) = match app.stage {
        Stage::Artist => (
            "artist",
            app.artist.current().map(|a| artist_lines(a, room)),
        ),
        Stage::Album => {
            let lines = app.album.current().map(|a| {
                let mut lines = album_lines(a, room);
                lines.push(Line::from(""));
                lines.push(Line::from(sp(
                    format!("your files · {}", app.slots.len()),
                    FG_FAINT,
                )));
                for slot in &app.slots {
                    lines.push(Line::from(sp(clip(&slot.title, room), FG_DIM)));
                }
                lines
            });
            ("album", lines)
        }
        Stage::Songs => {
            let local_ms = focused_slot(app).and_then(|s| s.existing.duration_ms);
            let lines = match app.focused_song() {
                Some(song) => Some(song_lines(song, local_ms, room)),
                None if app.pick.is_some() => None,
                None => Some(vec![Line::from(sp(
                    "no match yet · enter to search",
                    ACCENT,
                ))]),
            };
            ("omm match", lines)
        }
    };

    f.render_widget(
        Paragraph::new(Line::from(sp(title, FG_FAINT))),
        Rect { height: 1, ..inner },
    );
    let Some(lines) = lines else {
        f.render_widget(
            Paragraph::new(Line::from(sp("nothing selected", FG_FAINT))),
            Rect {
                y: inner.y + 2,
                height: 1,
                ..inner
            },
        );
        return;
    };

    let content = Rect {
        y: inner.y + 2,
        height: inner.height - 2,
        ..inner
    };
    let url = app.focused_artwork();
    let font = app.picker.font_size();
    let (fw, fh) = (font.width.max(1) as u32, font.height.max(1) as u32);
    let rows = ((content.height / 2).min(MAX_COVER_ROWS) as u32).min(content.width as u32 * fw / fh)
        as u16;
    let reserve = url
        .as_ref()
        .is_some_and(|u| !matches!(app.thumbs.get(u), Some(Thumb::Missing)))
        && rows >= MIN_COVER_ROWS;

    let mut text_area = content;
    if reserve {
        let cols = (rows as u32 * fh / fw) as u16;
        if let Some(Thumb::Ready(image)) = url.as_ref().and_then(|u| app.thumbs.get_mut(u)) {
            f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
                Rect {
                    x: content.x,
                    y: content.y,
                    width: cols,
                    height: rows,
                },
                image.as_mut(),
            );
        }
        text_area = Rect {
            y: content.y + rows + 1,
            height: content.height.saturating_sub(rows + 1),
            ..content
        };
    }

    f.render_widget(Paragraph::new(lines), text_area);
}

fn crumbs(app: &App, width: usize) -> Line<'static> {
    let part = |index: usize, stage: Stage, label: &str, value: String| -> Vec<Span<'static>> {
        let active = app.stage == stage;
        let head = format!("{} {}", index, label);
        let mut spans = vec![if active {
            bold(head, ACCENT)
        } else {
            sp(head, FG_DIM)
        }];
        if !value.is_empty() {
            spans.push(sp(format!("  {}", value), FG));
        }
        spans
    };

    let budget = width.saturating_sub(40) / 2;
    let artist = app
        .chosen_artist
        .as_ref()
        .map(|a| clip(&a.attributes.name, budget))
        .unwrap_or_default();
    let album = app
        .chosen_album
        .as_ref()
        .map(|a| clip(&a.attributes.name, budget))
        .unwrap_or_default();
    let songs = format!(
        "{}/{}",
        app.slots.iter().filter(|s| s.song.is_some()).count(),
        app.slots.len()
    );

    let mut spans = part(1, Stage::Artist, "artist", artist);
    spans.push(sp("   ›   ", FG_FAINT));
    spans.extend(part(2, Stage::Album, "album", album));
    spans.push(sp("   ›   ", FG_FAINT));
    spans.extend(part(3, Stage::Songs, "songs", songs));
    Line::from(spans)
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    fill(f, area, app.panel());
    let inner = area.inner(Margin::new(2, 1));
    let group = &app.groups[app.gi];
    let room = inner.width as usize;

    let left = vec![
        sp("● ", ACCENT),
        bold("ufrume", FG),
        sp("    ", FG),
        bold("manual tagging", ACCENT),
    ];
    let right = format!(
        "folder {} / {}  ·  {} files",
        app.gi + 1,
        app.groups.len(),
        app.slots.len()
    );
    let used: usize = left.iter().map(|s| s.content.chars().count()).sum();
    let mut top = left;
    if used + right.chars().count() < room {
        top.push(sp(" ".repeat(room - used - right.chars().count()), FG));
        top.push(sp(right, FG_DIM));
    }

    let mut local = vec![sp(clip(&group.rel, room.saturating_sub(10)), FG)];
    let artist = mode(app.slots.iter().map(|s| s.existing.artist.as_deref()));
    let album = mode(app.slots.iter().map(|s| s.existing.album.as_deref()));
    let tags = [artist, album].into_iter().flatten().collect::<Vec<_>>();
    if !tags.is_empty() {
        local.push(sp(format!("   tags: {}", tags.join(" — ")), FG_FAINT));
    }

    f.render_widget(
        Paragraph::new(vec![Line::from(top), Line::from(local)]),
        inner,
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match app.stage {
        Stage::Artist | Stage::Album => &[
            ("type", "search"),
            ("↑↓", "move"),
            ("enter", "choose"),
            ("tab", "next"),
            ("shift-tab", "back"),
            ("ctrl-s", "save folder"),
        ],
        Stage::Songs if app.pick.is_some() => &[
            ("type", "search"),
            ("↑↓", "results"),
            ("enter", "use & next file"),
            ("pgup/pgdn", "file"),
            ("tab", "widen/narrow"),
            ("shift-tab", "back"),
            ("ctrl-s", "save folder"),
        ],
        Stage::Songs => &[
            ("↑↓", "move"),
            ("enter", "confirm"),
            ("shift-tab", "back"),
            ("ctrl-s", "save folder"),
        ],
    };

    let mut spans = Vec::new();
    for (key, label) in keys {
        spans.push(sp(format!("    {} ", key), ACCENT));
        spans.push(sp(*label, FG_FAINT));
    }
    spans.push(sp("    ctrl-c ", ACCENT));
    spans.push(sp("quit", FG_FAINT));
    if let Some((message, good)) = app.status.as_ref() {
        spans.push(sp("    ", FG));
        spans.push(sp(message.clone(), if *good { GREEN } else { ACCENT }));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn ui(f: &mut Frame, app: &mut App) {
    let [content, foot] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [header, _, crumb, _, body] = Layout::vertical([
        Constraint::Length(4),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(content.inner(Margin::new(2, 1)));

    draw_header(f, app, header);
    f.render_widget(Paragraph::new(crumbs(app, crumb.width as usize)), crumb);

    let panel = app.panel();

    let field = app
        .background
        .map_or(Color::Rgb(52, 52, 52), |bg| blend(bg, 14));

    let search_title = |app: &App| {
        let focus = focused_slot(app).map(|slot| {
            let length = slot
                .existing
                .duration_ms
                .map(|ms| format!(" · {}", clock(ms as u64)))
                .unwrap_or_default();
            format!("{}{}", slot.name, length)
        });
        let scope = match (
            app.pick.as_ref().is_some_and(|p| p.scoped),
            app.chosen_album.as_ref(),
            app.chosen_artist.as_ref(),
        ) {
            (true, Some(album), _) => format!("in {}", album.attributes.name),
            (true, None, Some(artist)) => format!("by {}", artist.attributes.name),
            _ => "everywhere".to_string(),
        };
        format!("search for {}  ·  {}", focus.unwrap_or_default(), scope)
    };

    if app.stage == Stage::Songs {
        if body.height < 18 {
            draw_table(f, app, body);
        } else {
            let [top, bottom] =
                Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)])
                    .spacing(1)
                    .areas(body);
            draw_table(f, app, top);
            if app.pick.is_some() {
                let title = search_title(app);
                let [left, right] =
                    Layout::horizontal([Constraint::Percentage(58), Constraint::Min(1)])
                        .spacing(2)
                        .areas(bottom);
                if let Some(pick) = app.pick.as_mut() {
                    draw_search(f, panel, left, &title, &mut pick.pane, song_row, field);
                }
                draw_detail(f, app, right);
            } else {
                draw_summary(f, app, bottom);
            }
        }
    } else {
        let [left, right] = Layout::horizontal([Constraint::Percentage(58), Constraint::Min(1)])
            .spacing(2)
            .areas(body);
        if app.stage == Stage::Artist {
            draw_search(
                f,
                panel,
                left,
                "search artist",
                &mut app.artist,
                artist_row,
                field,
            );
        } else {
            draw_search(
                f,
                panel,
                left,
                "search album",
                &mut app.album,
                album_row,
                field,
            );
        }
        draw_detail(f, app, right);
    }
    draw_footer(f, app, foot);
}

pub fn run(
    files: Vec<PathBuf>,
    force: bool,
    resolver: Arc<Resolver>,
) -> Result<Counts, Box<dyn std::error::Error>> {
    let groups = build_groups(files, force);
    if groups.is_empty() {
        println!("  nothing to tag, use --force to retag files that already have omm ids");
        return Ok(Counts::default());
    }

    let (tx, rx) = spawn_workers(resolver.clone());
    let background = detect_background();
    enable_raw_mode()?;
    let _restore = Restore;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut terminal: Term = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App::new(groups, tx, resolver, force, picker, background);
    while !app.quit {
        while let Ok(resp) = rx.try_recv() {
            app.on_resp(resp);
        }
        app.tick();
        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(FRAME)?
            && let TermEvent::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            app.on_key(key);
        }
    }

    Ok(app.counts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::{OmmAlbumAttributes, OmmArtistAttributes, OmmSongAttributes};
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn artist(name: &str) -> OmmArtist {
        OmmArtist {
            id: "omm:artist:1".to_string(),
            attributes: OmmArtistAttributes {
                name: name.to_string(),
                artwork_url: None,
                genres: Some(vec!["Dance".to_string()]),
            },
        }
    }

    fn album(name: &str) -> OmmAlbum {
        OmmAlbum {
            id: "omm:album:2".to_string(),
            attributes: OmmAlbumAttributes {
                name: name.to_string(),
                artist_names: vec!["Daft Punk".to_string(), "Daft Punk".to_string()],
                release_date: Some("2001-03-12".to_string()),
                artwork_url: None,
                track_count: Some(14),
            },
        }
    }

    fn song(name: &str) -> OmmSong {
        OmmSong {
            id: "omm:song:3".to_string(),
            attributes: OmmSongAttributes {
                name: name.to_string(),
                album_name: Some("Discovery".to_string()),
                artist_names: vec!["Daft Punk".to_string()],
                isrc: Some("GBDUW0000053".to_string()),
                track_number: Some(1),
                disc_number: Some(1),
                genres: Some(vec!["Dance".to_string()]),
                release_date: Some("2000-11-30".to_string()),
                artwork_url: None,
                duration_ms: Some(320_357),
            },
            relationships: None,
        }
    }

    fn slot(title: &str) -> Slot {
        Slot {
            path: PathBuf::from(format!("/music/{title}.mp3")),
            name: format!("{title}.mp3"),
            existing: ExistingMeta {
                title: Some(title.to_string()),
                artist: Some("Daft Punk".to_string()),
                album: Some("Discovery".to_string()),
                duration_ms: Some(320_000),
            },
            title: title.to_string(),
            variants: vec![title.to_string()],
            song: None,
            state: MatchState::Done,
        }
    }

    fn sample_app() -> (App, Receiver<Req>) {
        let (tx, rx) = mpsc::channel();
        let group = Group {
            rel: "Daft Punk/Discovery".to_string(),
            has_artist: true,
            has_album: true,
            slots: vec![slot("One More Time"), slot("Aerodynamic")],
            artist_query: "Daft Punk".to_string(),
            album_query: "Discovery".to_string(),
        };
        let resolver = Arc::new(Resolver::new(reqwest::blocking::Client::new()));
        let mut app = App::new(
            vec![group],
            tx,
            resolver,
            false,
            Picker::halfblocks(),
            Some((20, 20, 20)),
        );
        app.artist.apply(Ok(vec![
            artist("Daft Punk"),
            artist("Daft Punk Experience"),
        ]));
        app.album.apply(Ok(vec![album("Discovery")]));
        (app, rx)
    }

    fn render(app: &mut App, width: u16, height: u16) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui(f, app)).unwrap();
    }

    #[test]
    fn input_edits_text() {
        let mut input = Input::new("daft");
        assert!(input.handle(&key(KeyCode::Char('!'))));
        assert_eq!(input.value(), "daft!");
        input.handle(&key(KeyCode::Left));
        input.handle(&key(KeyCode::Backspace));
        assert_eq!(input.value(), "daf!");
        input.handle(&KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(input.value(), "!");
    }

    #[test]
    fn walks_all_stages_and_renders() {
        let (mut app, _rx) = sample_app();
        for (w, h) in [(120, 34), (80, 24), (50, 16)] {
            render(&mut app, w, h);
        }

        app.on_key(key(KeyCode::Enter));
        assert!(app.stage == Stage::Album);
        assert_eq!(
            app.chosen_artist.as_ref().unwrap().attributes.name,
            "Daft Punk"
        );
        render(&mut app, 100, 30);

        app.album.apply(Ok(vec![album("Discovery")]));
        app.on_key(key(KeyCode::Enter));
        assert!(app.stage == Stage::Songs);
        assert!(app.slots.iter().all(|s| s.state == MatchState::Pending));
        assert!(app.pick.is_some());

        let gen_id = app.gen_id;
        app.on_resp(Resp::Matched {
            gen_id,
            slot: 0,
            result: Ok(Some(Box::new(song("One More Time")))),
        });
        app.on_resp(Resp::Matched {
            gen_id: gen_id - 1,
            slot: 1,
            result: Ok(Some(Box::new(song("stale")))),
        });
        assert!(app.slots[0].song.is_some());
        assert!(app.slots[1].song.is_none());
        for (w, h) in [(100, 30), (50, 16), (120, 50), (80, 24)] {
            render(&mut app, w, h);
        }

        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.cursor, 1);
        app.pick
            .as_mut()
            .unwrap()
            .pane
            .apply(Ok(vec![song("Aerodynamic")]));
        render(&mut app, 100, 30);
        app.on_key(key(KeyCode::Enter));
        assert!(app.slots[1].song.is_some());
        assert_eq!(app.cursor, 2);
        assert!(app.pick.is_none());
        render(&mut app, 100, 30);
    }

    #[test]
    fn changing_artist_resets_album() {
        let (mut app, _rx) = sample_app();
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Esc));
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert!(app.chosen_album.is_none());
        assert!(app.album.results.is_empty());
    }

    #[test]
    fn groups_files_by_folder_and_picks_common_tags() {
        let values = [Some("A"), Some("B"), Some("B"), None];
        assert_eq!(mode(values.into_iter()).as_deref(), Some("B"));
    }

    #[test]
    fn title_variants_cover_track_numbers_and_artist_prefixes() {
        let (title, variants) = title_variants(None, "01 - One More Time");
        assert_eq!(title, "One More Time");
        assert_eq!(variants, ["01 - One More Time", "One More Time"]);

        let (title, variants) = title_variants(None, "03. Daft Punk - Aerodynamic");
        assert_eq!(title, "Daft Punk - Aerodynamic");
        assert!(variants.contains(&"Aerodynamic".to_string()));

        let (title, variants) = title_variants(None, "07 Digital Love");
        assert_eq!(title, "07 Digital Love");
        assert!(variants.contains(&"Digital Love".to_string()));

        let (title, variants) = title_variants(Some("99 Problems"), "whatever");
        assert_eq!(title, "99 Problems");
        assert_eq!(variants[0], "99 Problems");
    }

    #[test]
    fn songs_without_album_are_scoped_to_the_artist() {
        let (mut app, rx) = sample_app();
        while rx.try_recv().is_ok() {}
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::Tab));
        assert!(app.stage == Stage::Songs);
        let mut matches = 0;
        while let Ok(req) = rx.try_recv() {
            if let Req::Match { hint, .. } = req {
                matches += 1;
                assert_eq!(hint.artist_id.as_deref(), Some("omm:artist:1"));
                assert!(hint.album_id.is_none());
            }
        }
        assert_eq!(matches, 2);
    }

    #[test]
    fn shift_tab_goes_back_and_tab_keeps_choices() {
        let (mut app, _rx) = sample_app();
        app.on_key(key(KeyCode::Enter));
        app.album.apply(Ok(vec![album("Discovery")]));
        app.on_key(key(KeyCode::Enter));
        assert!(app.stage == Stage::Songs);
        app.on_key(key(KeyCode::BackTab));
        assert!(app.stage == Stage::Album);
        app.on_key(key(KeyCode::BackTab));
        assert!(app.stage == Stage::Artist);
        app.on_key(key(KeyCode::Tab));
        app.on_key(key(KeyCode::Tab));
        assert!(app.stage == Stage::Songs);
        assert!(app.chosen_artist.is_some() && app.chosen_album.is_some());
    }

    #[test]
    fn discovery_fills_suggestions_when_tags_are_missing() {
        let (mut app, _rx) = sample_app();
        app.groups[0].has_artist = false;
        app.artist = Pane::new("Folder Name");
        app.on_resp(Resp::Discovered {
            group_seq: app.group_seq,
            discovery: Discovery {
                artists: vec![crate::tag::Suggestion {
                    item: artist("Daft Punk"),
                    votes: 2,
                    total: 2,
                }],
                albums: Vec::new(),
            },
        });
        assert!(app.artist.suggested);
        assert_eq!(app.artist.notes, ["2/2 files"]);
        render(&mut app, 100, 30);
    }

    #[test]
    #[ignore = "needs network"]
    fn discovers_album_from_titles_alone() {
        let resolver = Resolver::new(reqwest::blocking::Client::new());
        let probes: Vec<Probe> = [
            "One More Time",
            "Aerodynamic",
            "Digital Love",
            "Harder Better Faster Stronger",
        ]
        .iter()
        .map(|t| Probe {
            variants: vec![t.to_string()],
            hint: SongHint::default(),
        })
        .collect();
        let discovery = resolver.discover(&probes);
        let top = discovery.albums.first().expect("an album");
        assert_eq!(top.votes, 4);
        assert_eq!(top.item.attributes.name, "Discovery");
        assert_eq!(discovery.artists[0].item.attributes.name, "Daft Punk");
    }

    #[test]
    fn typing_on_the_songs_page_edits_the_search() {
        let (mut app, _rx) = sample_app();
        app.on_key(key(KeyCode::Enter));
        app.album.apply(Ok(vec![album("Discovery")]));
        app.on_key(key(KeyCode::Enter));
        assert!(app.stage == Stage::Songs);
        for c in "qwsx".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        assert!(!app.quit);
        assert!(app.stage == Stage::Songs);
        assert_eq!(app.pick.as_ref().unwrap().pane.query(), "One More Timeqwsx");
        assert_eq!(app.counts.tagged + app.counts.skipped, 0);
    }

    #[test]
    fn skip_row_moves_to_next_folder() {
        let (mut app, _rx) = sample_app();
        app.on_key(key(KeyCode::Enter));
        app.album.apply(Ok(vec![album("Discovery")]));
        app.on_key(key(KeyCode::Enter));
        app.on_key(key(KeyCode::PageDown));
        app.on_key(key(KeyCode::PageDown));
        app.on_key(key(KeyCode::PageDown));
        assert!(app.pick.is_none());
        render(&mut app, 100, 30);
        app.on_key(key(KeyCode::Enter));
        assert!(app.quit);
        assert_eq!(app.counts.skipped, 2);
    }

    #[test]
    fn ctrl_s_saves_with_only_an_artist() {
        let (mut app, _rx) = sample_app();
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        app.on_key(ctrl_s);
        assert!(!app.quit);
        assert!(
            app.status
                .as_ref()
                .is_some_and(|(m, _)| m.contains("pick an artist"))
        );

        app.on_key(key(KeyCode::Enter));
        assert!(app.chosen_artist.is_some() && app.chosen_album.is_none());
        assert_eq!(app.writable(), 2);
        app.on_key(ctrl_s);
        assert!(app.quit);
        assert_eq!(app.counts.failed, 2);
    }
}
