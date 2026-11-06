use crate::tag::{Counts, Event, Report};

use console::truncate_str;
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event as TermEvent, KeyCode, KeyEventKind,
        KeyModifiers, MouseEventKind,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use image::DynamicImage;
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Margin, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Paragraph},
};
use ratatui_image::{
    FilterType, Resize, StatefulImage, picker::Picker, protocol::StatefulProtocol,
};
use std::collections::VecDeque;
use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::{Duration, Instant};
use terminal_colorsaurus::{QueryOptions, background_color};

pub(crate) const ACCENT: Color = Color::Rgb(255, 135, 31);
pub(crate) const GREEN: Color = Color::Rgb(126, 200, 120);
pub(crate) const RED: Color = Color::Rgb(235, 100, 100);
pub(crate) const FG: Color = Color::Rgb(216, 216, 216);
pub(crate) const FG_DIM: Color = Color::Rgb(138, 138, 138);
pub(crate) const FG_FAINT: Color = Color::Rgb(86, 86, 86);

const LOG_CAPACITY: usize = 500;
const EVENTS_PER_FRAME: usize = 256;
pub(crate) const FRAME: Duration = Duration::from_millis(50);
const MAX_COVER_ROWS: u16 = 10;
const MIN_COVER_ROWS: u16 = 4;
const LABEL_WIDTH: usize = 10;

pub(crate) type Term = Terminal<CrosstermBackend<io::Stdout>>;

enum Kind {
    Tagged,
    Skipped,
    Failed,
}

struct Entry {
    kind: Kind,
    main: String,
    extra: String,
}

struct Last {
    rel: String,
    report: Report,
    image: Option<StatefulProtocol>,
}

struct App {
    total: usize,
    counts: Counts,
    started: Instant,
    finished: Option<Instant>,
    current: Option<String>,
    last: Option<Last>,
    log: VecDeque<Entry>,
    scroll: usize,
    log_rows: usize,
    picker: Picker,
    background: Option<(u8, u8, u8)>,
}

pub(crate) struct Restore;

impl Drop for Restore {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen);
    }
}

pub(crate) fn blend(bg: (u8, u8, u8), alpha: i32) -> Color {
    let luma = (bg.0 as i32 * 299 + bg.1 as i32 * 587 + bg.2 as i32 * 114) / 1000;
    let target = if luma < 128 { 255 } else { 0 };
    let mix = |c: u8| (c as i32 + (target - c as i32) * alpha / 100) as u8;
    Color::Rgb(mix(bg.0), mix(bg.1), mix(bg.2))
}

pub(crate) fn sp(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(text.into(), Style::default().fg(color))
}

pub(crate) fn bold(text: impl Into<String>, color: Color) -> Span<'static> {
    Span::styled(
        text.into(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )
}

pub(crate) fn clip(text: &str, width: usize) -> String {
    truncate_str(text, width.max(1), "…").into_owned()
}

fn width(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.content.chars().count()).sum()
}

fn format_number(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\'');
        }
        out.push(c);
    }
    out
}

fn format_clock(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

impl App {
    fn done(&self) -> usize {
        self.counts.tagged + self.counts.unmatched + self.counts.skipped + self.counts.failed
    }

    fn elapsed(&self) -> Duration {
        self.finished.unwrap_or_else(Instant::now) - self.started
    }

    fn pace(&self) -> f64 {
        let secs = self.elapsed().as_secs_f64();
        if secs < 0.5 {
            0.0
        } else {
            self.done() as f64 / secs
        }
    }

    fn eta(&self) -> Option<u64> {
        let pace = self.pace();
        (pace > 0.0 && self.finished.is_none())
            .then(|| (self.total.saturating_sub(self.done()) as f64 / pace) as u64)
    }

    fn panel(&self) -> Color {
        self.background.map_or(Color::Reset, |bg| blend(bg, 5))
    }

    fn push_log(&mut self, entry: Entry) {
        self.log.push_front(entry);
        self.log.truncate(LOG_CAPACITY);
        if self.scroll > 0 {
            self.scroll += 1;
        }
    }

    fn max_scroll(&self) -> usize {
        self.log.len().saturating_sub(self.log_rows)
    }

    fn scroll_by(&mut self, delta: isize) {
        let next = self.scroll.saturating_add_signed(delta);
        self.scroll = next.min(self.max_scroll());
    }

    fn page(&self) -> isize {
        self.log_rows.saturating_sub(1).max(1) as isize
    }

    fn handle(&mut self, event: Event) {
        match event {
            Event::Started(rel) => self.current = Some(rel),
            Event::Tagged { rel, report, cover } => {
                self.counts.tagged += 1;
                self.push_log(Entry {
                    kind: Kind::Tagged,
                    main: report.candidate.title.clone(),
                    extra: report.candidate.artists.join(", "),
                });
                let image =
                    cover.map(|c: Arc<DynamicImage>| self.picker.new_resize_protocol((*c).clone()));
                self.last = Some(Last {
                    rel,
                    report: *report,
                    image,
                });
            }
            Event::Skipped { rel, reason } => {
                if reason.is_unmatched() {
                    self.counts.unmatched += 1;
                } else {
                    self.counts.skipped += 1;
                }
                self.push_log(Entry {
                    kind: Kind::Skipped,
                    main: rel,
                    extra: reason.label().to_string(),
                });
            }
            Event::Failed { rel, error } => {
                self.counts.failed += 1;
                self.push_log(Entry {
                    kind: Kind::Failed,
                    main: rel,
                    extra: error,
                });
            }
            Event::Finished => {
                self.finished = Some(Instant::now());
                self.current = None;
            }
        }
    }
}

pub(crate) fn fill(f: &mut Frame, area: Rect, bg: Color) {
    f.render_widget(Block::default().style(Style::default().bg(bg)), area);
}

pub(crate) fn detect_background() -> Option<(u8, u8, u8)> {
    let mut options = QueryOptions::default();
    options.timeout = Duration::from_millis(300);
    background_color(options).ok().map(|c| c.scale_to_8bit())
}

pub fn run(
    total: usize,
    rx: &Receiver<Event>,
    stop: Arc<AtomicBool>,
) -> Result<Counts, Box<dyn std::error::Error>> {
    let background = detect_background();
    enable_raw_mode()?;
    let _restore = Restore;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    let mut terminal: Term = Terminal::new(CrosstermBackend::new(stdout))?;

    let mut app = App {
        total,
        counts: Counts::default(),
        started: Instant::now(),
        finished: None,
        current: None,
        last: None,
        log: VecDeque::new(),
        scroll: 0,
        log_rows: 0,
        picker,
        background,
    };

    let mut disconnected = false;
    loop {
        for _ in 0..EVENTS_PER_FRAME {
            match rx.try_recv() {
                Ok(event) => app.handle(event),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        if disconnected && app.finished.is_none() {
            app.finished = Some(Instant::now());
            app.current = None;
        }

        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(FRAME)? {
            match event::read()? {
                TermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                    let ctrl_c = key.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(key.code, KeyCode::Char('c'));
                    if ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc) {
                        stop.store(true, Ordering::Relaxed);
                        break;
                    }
                    match key.code {
                        KeyCode::Up | KeyCode::Char('k') => app.scroll_by(-1),
                        KeyCode::Down | KeyCode::Char('j') => app.scroll_by(1),
                        KeyCode::PageUp => app.scroll_by(-app.page()),
                        KeyCode::PageDown => app.scroll_by(app.page()),
                        KeyCode::Home | KeyCode::Char('g') => app.scroll = 0,
                        KeyCode::End | KeyCode::Char('G') => app.scroll = app.max_scroll(),
                        _ => {}
                    }
                }
                TermEvent::Mouse(mouse) => match mouse.kind {
                    MouseEventKind::ScrollUp => app.scroll_by(-3),
                    MouseEventKind::ScrollDown => app.scroll_by(3),
                    _ => {}
                },
                _ => {}
            }
        }
    }

    Ok(app.counts)
}

fn ui(f: &mut Frame, app: &mut App) {
    let [content, foot] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
    let [header, _, tiles, _, progress, _, body] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(6),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .areas(content.inner(Margin::new(2, 1)));

    draw_header(f, app, header);
    draw_tiles(f, app, tiles);
    draw_progress(f, app, progress);

    let [now, log] = Layout::horizontal([Constraint::Percentage(60), Constraint::Min(1)])
        .spacing(2)
        .areas(body);
    draw_now(f, app, now);
    draw_log(f, app, log);
    draw_footer(f, app, foot);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    fill(f, area, app.panel());
    let area = area.inner(Margin::new(2, 1));

    let mut left = vec![sp("● ", ACCENT), bold("ufrume", FG), sp("    ", FG)];
    left.push(bold(
        if app.finished.is_some() {
            "done"
        } else {
            "tagging"
        },
        ACCENT,
    ));

    let right = vec![
        bold(format_number(app.done()), FG),
        sp(format!(" / {} files    ", format_number(app.total)), FG_DIM),
        sp(format_clock(app.elapsed().as_secs()), FG_DIM),
    ];

    let mut spans = left;
    if width(&spans) + width(&right) <= area.width as usize {
        let pad = (area.width as usize) - width(&spans) - width(&right);
        spans.push(sp(" ".repeat(pad), FG));
        spans.extend(right);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_tile(
    f: &mut Frame,
    panel: Color,
    area: Rect,
    title: &str,
    value: String,
    label: &str,
    tint: Color,
) {
    fill(f, area, panel);
    let inner = area.inner(Margin::new(2, 1));
    let lines = vec![
        Line::from(bold(title, FG)),
        Line::from(""),
        Line::from(bold(value, tint)),
        Line::from(sp(clip(label, inner.width as usize), FG_DIM)),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn draw_tiles(f: &mut Frame, app: &App, area: Rect) {
    let cols: [Rect; 5] = Layout::horizontal([Constraint::Ratio(1, 5); 5])
        .spacing(2)
        .areas(area);
    let panel = app.panel();
    let c = app.counts;

    draw_tile(
        f,
        panel,
        cols[0],
        "tagged",
        format_number(c.tagged),
        "matched and written",
        FG,
    );
    draw_tile(
        f,
        panel,
        cols[1],
        "unmatched",
        format_number(c.unmatched),
        "not in the catalog",
        if c.unmatched > 0 { ACCENT } else { FG },
    );
    draw_tile(
        f,
        panel,
        cols[2],
        "skipped",
        format_number(c.skipped),
        "already tagged",
        FG,
    );
    draw_tile(
        f,
        panel,
        cols[3],
        "failed",
        format_number(c.failed),
        "see log",
        if c.failed > 0 { RED } else { FG },
    );
    let eta = app
        .eta()
        .map(|s| format!("eta {}", format_clock(s)))
        .unwrap_or_else(|| "files per second".to_string());
    draw_tile(
        f,
        panel,
        cols[4],
        "pace",
        format!("{:.1}/s", app.pace()),
        &eta,
        ACCENT,
    );
}

fn draw_progress(f: &mut Frame, app: &App, area: Rect) {
    let fraction = if app.total == 0 {
        1.0
    } else {
        app.done() as f64 / app.total as f64
    };
    let suffix = format!("  {:>5.1}%", fraction * 100.0);
    let bar = (area.width as usize).saturating_sub(suffix.chars().count());
    let filled = ((bar as f64) * fraction).round() as usize;

    f.render_widget(
        Paragraph::new(Line::from(vec![
            sp("━".repeat(filled), ACCENT),
            sp("━".repeat(bar.saturating_sub(filled)), FG_FAINT),
            sp(suffix, FG_DIM),
        ])),
        area,
    );
}

fn field(label: &str, value: Vec<Span<'static>>) -> Line<'static> {
    let mut spans = vec![sp(format!("{:<w$}", label, w = LABEL_WIDTH), FG_DIM)];
    spans.extend(value);
    Line::from(spans)
}

fn detail_lines(last: &Last, room: usize) -> Vec<Line<'static>> {
    let c = &last.report.candidate;
    let value_room = room.saturating_sub(LABEL_WIDTH);
    let text = |s: &str, color: Color| sp(clip(s, value_room), color);

    let mut lines = vec![
        field("file", vec![text(&last.rel, FG_DIM)]),
        Line::from(""),
        field(
            "title",
            vec![Span::styled(
                clip(&c.title, value_room),
                Style::default().fg(FG).add_modifier(Modifier::BOLD),
            )],
        ),
        field("artist", vec![text(&c.artists.join(", "), FG)]),
    ];

    if let Some(ref album) = c.album {
        lines.push(field("album", vec![text(album, FG)]));
    }

    let mut release: Vec<String> = Vec::new();
    if let Some(ref date) = c.date {
        release.push(date.clone());
    }
    if let Some(track) = c.track {
        release.push(format!("track {}", track));
    }
    if let Some(disc) = c.disc {
        release.push(format!("disc {}", disc));
    }
    if !release.is_empty() {
        lines.push(field("release", vec![text(&release.join("  ·  "), FG)]));
    }
    if let Some(ref genre) = c.genre {
        lines.push(field("genre", vec![text(genre, FG)]));
    }
    if let Some(ref isrc) = c.isrc {
        lines.push(field("isrc", vec![text(isrc, FG_DIM)]));
    }

    lines.push(Line::from(""));
    if last.report.changed.is_empty() {
        lines.push(field(
            "changed",
            vec![text("nothing, tags already matched", FG_FAINT)],
        ));
    } else {
        lines.push(field(
            "changed",
            vec![text(&last.report.changed.join(", "), ACCENT)],
        ));
    }
    if !last.report.wrote.is_empty() {
        lines.push(field(
            "wrote",
            vec![text(&last.report.wrote.join(", "), GREEN)],
        ));
    }

    lines
}

fn draw_now(f: &mut Frame, app: &mut App, area: Rect) {
    fill(f, area, app.panel());
    let inner = area.inner(Margin::new(2, 1));
    if inner.height < 3 {
        return;
    }

    f.render_widget(
        Paragraph::new(Line::from(sp("latest match", FG_FAINT))),
        Rect { height: 1, ..inner },
    );

    let status = Rect {
        y: inner.y + inner.height - 1,
        height: 1,
        ..inner
    };
    let status_line = match (&app.current, app.finished.is_some()) {
        (Some(current), _) => Line::from(vec![
            sp("▸ ", ACCENT),
            sp(
                clip(current, (inner.width as usize).saturating_sub(2)),
                FG_DIM,
            ),
        ]),
        (None, true) => Line::from(sp("✓ finished", GREEN)),
        _ => Line::from(""),
    };
    f.render_widget(Paragraph::new(status_line), status);

    let content = Rect {
        y: inner.y + 2,
        height: inner.height.saturating_sub(4),
        ..inner
    };

    let font = app.picker.font_size();
    let (fw, fh) = (font.width.max(1) as u32, font.height.max(1) as u32);

    let Some(last) = app.last.as_mut() else {
        f.render_widget(
            Paragraph::new(Line::from(sp("waiting for the first match", FG_FAINT))),
            content,
        );
        return;
    };

    let spare = content.width.saturating_sub(2 + LABEL_WIDTH as u16 + 12) as u32;
    let rows = content
        .height
        .min(MAX_COVER_ROWS)
        .min((spare * fw / fh) as u16);
    let cols = (rows as u32 * fh / fw) as u16;
    let show_cover = last.image.is_some() && rows >= MIN_COVER_ROWS;

    let text_area = if show_cover {
        let cover = Rect {
            x: content.x,
            y: content.y,
            width: cols,
            height: rows,
        };
        if let Some(image) = last.image.as_mut() {
            f.render_stateful_widget(
                StatefulImage::default().resize(Resize::Scale(Some(FilterType::Triangle))),
                cover,
                image,
            );
        }
        Rect {
            x: content.x + cols + 2,
            width: content.width - cols - 2,
            ..content
        }
    } else {
        content
    };

    let lines = detail_lines(last, text_area.width as usize);
    f.render_widget(Paragraph::new(lines), text_area);
}

fn draw_log(f: &mut Frame, app: &mut App, area: Rect) {
    fill(f, area, app.panel());
    let inner = area.inner(Margin::new(2, 1));
    if inner.height < 3 {
        return;
    }

    app.log_rows = inner.height.saturating_sub(2) as usize;
    app.scroll = app.scroll.min(app.max_scroll());

    let mut heading = vec![sp("log", FG_FAINT)];
    if app.scroll > 0 {
        heading.push(sp(
            format!("   ↑ {} newer hidden, Home to follow", app.scroll),
            ACCENT,
        ));
    }
    f.render_widget(
        Paragraph::new(Line::from(heading)),
        Rect { height: 1, ..inner },
    );

    let room = inner.width as usize;
    let lines: Vec<Line> = app
        .log
        .iter()
        .skip(app.scroll)
        .take(app.log_rows)
        .map(|entry| {
            let (marker, color) = match entry.kind {
                Kind::Tagged => ("✓ ", GREEN),
                Kind::Skipped => ("· ", FG_FAINT),
                Kind::Failed => ("✗ ", RED),
            };
            let main_color = match entry.kind {
                Kind::Tagged => FG,
                Kind::Skipped => FG_DIM,
                Kind::Failed => FG,
            };
            let main = clip(&entry.main, room.saturating_sub(2));
            let used = 2 + main.chars().count();
            let extra = if used + 4 < room {
                clip(&entry.extra, room - used - 3)
            } else {
                String::new()
            };

            let mut spans = vec![sp(marker, color), sp(main, main_color)];
            if !extra.is_empty() {
                spans.push(sp("  ", FG));
                spans.push(sp(extra, FG_FAINT));
            }
            Line::from(spans)
        })
        .collect();

    f.render_widget(
        Paragraph::new(lines),
        Rect {
            y: inner.y + 2,
            height: inner.height - 2,
            ..inner
        },
    );
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![
        sp("    q ", ACCENT),
        sp("quit", FG_FAINT),
        sp("    ↑↓ ", ACCENT),
        sp("scroll log", FG_FAINT),
    ];
    if app.finished.is_some() {
        spans.push(sp("    all files processed", FG_FAINT));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tag::TagCandidate;
    use ratatui::backend::TestBackend;

    fn sample_app() -> App {
        let picker = Picker::halfblocks();
        let image = DynamicImage::new_rgb8(160, 160);
        let mut app = App {
            total: 3401,
            counts: Counts::default(),
            started: Instant::now() - Duration::from_secs(23),
            finished: None,
            current: Some("Inner Circle/Rock With You (5-Inch)/02 Rock With You.mp3".to_string()),
            last: None,
            log: VecDeque::new(),
            scroll: 0,
            log_rows: 0,
            picker,
            background: Some((39, 43, 54)),
        };
        app.handle(Event::Skipped {
            rel: "Dune/Forever/01 Intro.mp3".to_string(),
            reason: crate::tag::SkipReason::NoMatch,
        });
        app.handle(Event::Failed {
            rel: "Dune/Forever/02 Broken.mp3".to_string(),
            error: "API unavailable (503)".to_string(),
        });
        app.handle(Event::Tagged {
            rel: "Inner Circle/Rock With You (5-Inch)/01 Rock With You.mp3".to_string(),
            report: Box::new(Report {
                candidate: TagCandidate {
                    title: "Rock with You".to_string(),
                    artists: vec!["Inner Circle".to_string()],
                    album: Some("Bad to the Bone".to_string()),
                    album_artists: vec!["Inner Circle".to_string()],
                    date: Some("1992".to_string()),
                    track: Some(4),
                    disc: Some(1),
                    genre: Some("Reggae".to_string()),
                    isrc: Some("USXXX9200001".to_string()),
                    artwork: None,
                    song_id: "omm:song:1".to_string(),
                    album_id: Some("omm:album:2".to_string()),
                    artist_ids: vec!["omm:artist:3".to_string()],
                },
                changed: vec!["title", "album"],
                wrote: vec!["date", "track", "disc", "genre", "isrc"],
            }),
            cover: Some(Arc::new(image)),
        });
        app
    }

    #[test]
    fn renders() {
        for (w, h) in [(120, 34), (80, 24), (50, 16)] {
            let mut app = sample_app();
            let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
            term.draw(|f| ui(f, &mut app)).unwrap();
            let buf = term.backend().buffer().clone();
            let screen: String = (0..h)
                .flat_map(|y| (0..w).map(move |x| (x, y)))
                .map(|(x, y)| buf[(x, y)].symbol().to_string())
                .collect();
            assert!(screen.contains("ufrume"));
            assert!(screen.contains("quit"));
            if w >= 100 {
                assert!(screen.contains("Rock with You"));
            }
        }
    }

    #[test]
    fn scrolls_and_clamps() {
        let mut app = sample_app();
        for i in 0..40 {
            app.handle(Event::Skipped {
                rel: format!("track {i}.mp3"),
                reason: crate::tag::SkipReason::NoMatch,
            });
        }
        let mut term = Terminal::new(TestBackend::new(120, 34)).unwrap();
        term.draw(|f| ui(f, &mut app)).unwrap();
        assert!(app.log_rows > 0);

        app.scroll_by(1000);
        assert_eq!(app.scroll, app.max_scroll());
        app.scroll_by(-1000);
        assert_eq!(app.scroll, 0);

        app.scroll_by(5);
        app.handle(Event::Skipped {
            rel: "late.mp3".to_string(),
            reason: crate::tag::SkipReason::NoMatch,
        });
        assert_eq!(app.scroll, 6);
    }
}
