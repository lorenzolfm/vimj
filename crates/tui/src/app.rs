//! TUI state and input. Vim-style: a normal mode with counts, motions and
//! operators (`5j`, `dd`, `yy`, `p`), a visual line mode (`V`), a command
//! line (`:`) and search (`/`, `n`, `N`). The mouse works too.

use crate::client::Client;
use crate::editor::{self, Action, Editor, Field};
use crate::mouse::{Clicks, Hits, Target};
use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vimj_core::protocol::*;
use vimj_core::{Preset, Transition};
use ratatui::layout::Rect;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

const TOAST_FOR: Duration = Duration::from_secs(5);
const SEEK_MS: f64 = 10_000.0;
const LONG_SEEK_MS: f64 = 30_000.0;
const VOLUME_STEP: i16 = 5;
/// Rows kept between the cursor and the edge of the list as keys move it.
const SCROLL_MARGIN: usize = 3;
/// Rows one turn of the wheel scrolls.
const SCROLL_LINES: isize = 3;

#[allow(clippy::large_enum_variant)]
pub enum Msg {
    Event(Event),
    Connected(bool),
    Playlists(Playlists),
    View(PlaylistView),
    Suggestions { playlist: String, anchor: usize, list: Vec<Suggestion> },
    Transition(TransitionDetail),
    /// `:w` stored `sent`; `close` shuts the editor now.
    Saved { detail: TransitionDetail, sent: Transition, close: bool },
    Waveform(String, Option<Vec<u8>>),
    Player(PlayerState),
    Downloads(Vec<Download>),
    /// A playlist was made; add these songs to it and show it.
    Created(PlaylistSummary, Vec<String>),
    Info(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Main,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    Playlist(String),
    Downloads,
}

#[derive(Clone, Debug, Default)]
pub struct LineInput {
    pub text: String,
    /// Byte offset.
    pub cursor: usize,
}

impl LineInput {
    fn new(text: &str) -> LineInput {
        LineInput { text: text.to_owned(), cursor: text.len() }
    }

    /// Returns false when the key isn't line editing.
    fn key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.text.len(),
            KeyCode::Char('u') if ctrl => {
                self.text.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let before = self.text[..self.cursor].trim_end();
                let start = before.rfind(' ').map_or(0, |i| i + 1);
                self.text.drain(start..self.cursor);
                self.cursor = start;
            }
            KeyCode::Char(c) if !ctrl => {
                self.text.insert(self.cursor, c);
                self.cursor += c.len_utf8();
            }
            KeyCode::Backspace => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Delete => {
                if self.cursor < self.text.len() {
                    self.text.remove(self.cursor);
                }
            }
            KeyCode::Left => {
                if let Some(c) = self.text[..self.cursor].chars().next_back() {
                    self.cursor -= c.len_utf8();
                }
            }
            KeyCode::Right => {
                if let Some(c) = self.text[self.cursor..].chars().next() {
                    self.cursor += c.len_utf8();
                }
            }
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.text.len(),
            _ => return false,
        }
        true
    }

    fn paste(&mut self, text: &str) {
        let clean: String = text.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &clean);
        self.cursor += clean.len();
    }
}

#[derive(Clone, Debug)]
pub enum Confirm {
    DeleteSongs(Vec<(String, String)>),
    DeletePlaylist(String, String),
}

pub enum Mode {
    Normal,
    Command(LineInput),
    Search { input: LineInput, origin: usize },
    Confirm(Confirm),
}

pub struct SuggestionsPopup {
    pub playlist: String,
    pub anchor: usize,
    pub anchor_title: String,
    pub items: Option<Vec<Suggestion>>,
    pub cursor: usize,
}

pub struct AddToPopup {
    pub songs: Vec<String>,
    pub cursor: usize,
}

pub enum Popup {
    Suggestions(SuggestionsPopup),
    AddTo(AddToPopup),
    Help,
}

/// What `p` puts.
#[derive(Clone, Debug)]
pub struct Register {
    pub songs: Vec<String>,
}

/// What dragging with the button held does.
#[derive(Clone, Copy, Debug)]
enum Drag {
    /// Selects songs from this row.
    Select { anchor: usize },
    /// Moves a cue point in the transition editor, from where it was when
    /// the button went down at `column`.
    Cue { field: Field, column: u16, from_ms: f64, ms_per_column: f64 },
    /// Moves the picked point of a curve being drawn, over `area`.
    Point { area: Rect, t0: f64, ms_per_col: f64, duration_ms: f64 },
}

pub struct App {
    client: Client,
    tx: UnboundedSender<Msg>,
    pub quit: bool,
    pub stop_daemon: bool,
    pub connected: bool,
    pub status: Option<Status>,
    pub playlists: Option<Playlists>,
    pub focus: Focus,
    /// Sidebar cursor: library, downloads, then playlists.
    pub side: usize,
    pub view: View,
    pub list: Option<PlaylistView>,
    pub cursor: usize,
    /// Song to put the cursor on when the view next loads.
    cursor_target: Option<String>,
    pub offset: usize,
    /// Rows kept round the cursor when scrolling: none once the mouse has
    /// moved it, so a click doesn't scroll the row out from under it.
    pub margin: usize,
    /// Rows visible in the list, and where it is on screen.
    pub page: usize,
    pub list_area: Rect,
    /// What the last frame drew where.
    pub hits: Hits,
    clicks: Clicks,
    drag: Option<Drag>,
    pub download_cursor: usize,
    pub visual: Option<usize>,
    pub mode: Mode,
    /// Normal-mode keys typed so far: a count and/or an operator.
    pub pending: String,
    pub register: Option<Register>,
    /// Songs to put in the playlist `:new` makes next.
    adding: Option<Vec<String>>,
    pub search: Option<String>,
    pub popup: Option<Popup>,
    pub editor: Option<Editor>,
    pub player: Option<PlayerState>,
    pub player_at: Instant,
    pub downloads: Vec<Download>,
    pub toast: Option<(Level, String, Instant)>,
    pub waveforms: HashMap<String, Arc<Vec<u8>>>,
    waveform_requested: HashSet<String>,
}

impl App {
    pub fn new(client: Client, tx: UnboundedSender<Msg>) -> App {
        App {
            client,
            tx,
            quit: false,
            stop_daemon: false,
            connected: false,
            status: None,
            playlists: None,
            focus: Focus::Main,
            side: 0,
            view: View::Playlist(LIBRARY_ID.into()),
            list: None,
            cursor: 0,
            cursor_target: None,
            offset: 0,
            margin: SCROLL_MARGIN,
            page: 20,
            list_area: Rect::default(),
            hits: Hits::default(),
            clicks: Clicks::default(),
            drag: None,
            download_cursor: 0,
            visual: None,
            mode: Mode::Normal,
            pending: String::new(),
            register: None,
            adding: None,
            search: None,
            popup: None,
            editor: None,
            player: None,
            player_at: Instant::now(),
            downloads: Vec::new(),
            toast: None,
            waveforms: HashMap::new(),
            waveform_requested: HashSet::new(),
        }
    }

    pub fn start(&mut self) {
        self.load_playlists();
        self.load_view();
    }

    pub async fn finish(&self) {
        if self.stop_daemon {
            let _ = self.client.shutdown().await;
        }
    }

    /// Runs a request in the background, turning its result into a message.
    fn spawn<T: Send + 'static>(&self, request: impl Future<Output = anyhow::Result<T>> + Send + 'static, done: impl FnOnce(T) -> Msg + Send + 'static) {
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let msg = match request.await {
                Ok(v) => done(v),
                Err(e) => Msg::Error(format!("{e:#}")),
            };
            let _ = tx.send(msg);
        });
    }

    fn client(&self) -> Client {
        self.client.clone()
    }

    fn info(&mut self, message: impl Into<String>) {
        self.toast = Some((Level::Info, message.into(), Instant::now()));
    }

    fn error(&mut self, message: impl Into<String>) {
        self.toast = Some((Level::Error, message.into(), Instant::now()));
    }

    // Loading.

    fn load_playlists(&self) {
        let c = self.client();
        self.spawn(async move { c.playlists().await }, Msg::Playlists);
    }

    fn load_view(&self) {
        match &self.view {
            View::Playlist(id) => {
                let (c, id) = (self.client(), id.clone());
                self.spawn(async move { c.playlist(&id).await }, Msg::View);
            }
            View::Downloads => {}
        }
    }

    pub fn current_id(&self) -> Option<&str> {
        match &self.view {
            View::Playlist(id) => Some(id),
            View::Downloads => None,
        }
    }

    pub fn songs_len(&self) -> usize {
        self.list.as_ref().filter(|l| Some(l.id.as_str()) == self.current_id()).map_or(0, |l| l.songs.len())
    }

    /// Sidebar rows: library, downloads, then each playlist.
    pub fn side_items(&self) -> Vec<(View, String)> {
        let mut items = vec![(View::Playlist(LIBRARY_ID.into()), "Library".to_owned()), (View::Downloads, "Downloads".to_owned())];
        if let Some(p) = &self.playlists {
            items.extend(p.playlists.iter().map(|p| (View::Playlist(p.id.clone()), p.name.clone())));
        }
        items
    }

    fn open(&mut self, view: View) {
        if self.view != view {
            self.view = view;
            self.cursor = 0;
            self.offset = 0;
            self.visual = None;
            self.list = None;
        }
        if let Some(at) = self.side_items().iter().position(|(v, _)| *v == self.view) {
            self.side = at;
        }
        self.load_view();
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Connected(up) => {
                self.connected = up;
                if up {
                    self.start();
                }
            }
            Msg::Event(e) => self.on_event(e),
            Msg::Playlists(p) => {
                self.playlists = Some(p);
                // A deleted playlist being shown falls back to the library.
                if let View::Playlist(id) = &self.view
                    && id != LIBRARY_ID
                    && !self.playlists.as_ref().is_some_and(|p| p.playlists.iter().any(|x| x.id == *id))
                {
                    self.open(View::Playlist(LIBRARY_ID.into()));
                }
                self.side = self.side.min(self.side_items().len() - 1);
            }
            Msg::View(view) => {
                if Some(view.id.as_str()) == self.current_id() {
                    if let Some(target) = self.cursor_target.take()
                        && let Some(at) = view.songs.iter().position(|s| s.song.id == target)
                    {
                        self.cursor = at;
                    }
                    self.cursor = self.cursor.min(view.songs.len().saturating_sub(1));
                    if let Some(anchor) = self.visual {
                        self.visual = Some(anchor.min(view.songs.len().saturating_sub(1)));
                    }
                    self.list = Some(view);
                }
            }
            Msg::Suggestions { playlist, anchor, list } => {
                if let Some(Popup::Suggestions(p)) = &mut self.popup
                    && p.playlist == playlist
                    && p.anchor == anchor
                {
                    p.items = Some(list);
                }
            }
            Msg::Transition(detail) => {
                if let Some(ed) = &mut self.editor {
                    let (from, to) = (detail.from.song.id.clone(), detail.to.song.id.clone());
                    ed.loaded(detail);
                    self.ensure_waveform(&from);
                    self.ensure_waveform(&to);
                }
            }
            Msg::Saved { detail, sent, close } => {
                self.info(format!("Saved the transition into {}: {}", detail.to.song.title, crate::theme::transition_summary(&detail.transition)));
                if let Some(ed) = &mut self.editor
                    && ed.is_pair(&detail)
                {
                    ed.saved(detail, &sent);
                    if close {
                        self.close_editor();
                    }
                }
            }
            Msg::Waveform(id, bytes) => {
                if let Some(bytes) = bytes {
                    self.waveforms.insert(id, Arc::new(bytes));
                } else {
                    // Not analysed yet; try again later.
                    self.waveform_requested.remove(&id);
                }
            }
            Msg::Player(p) => self.set_player(p),
            Msg::Downloads(d) => self.downloads = d,
            Msg::Created(summary, songs) => {
                self.info(format!("Made playlist {}", summary.name));
                self.load_playlists();
                if !songs.is_empty() {
                    let (c, id) = (self.client(), summary.id.clone());
                    self.spawn(async move { c.add_songs(&id, songs, None).await }, Msg::View);
                }
                self.open(View::Playlist(summary.id));
            }
            Msg::Info(m) => self.info(m),
            Msg::Error(m) => self.error(m),
        }
    }

    fn on_event(&mut self, event: Event) {
        match event {
            Event::Status(s) => self.status = Some(s),
            Event::Player(p) => self.set_player(p),
            Event::Library => {
                self.load_view();
                self.waveform_requested.retain(|id| self.waveforms.contains_key(id));
            }
            Event::Playlists => self.load_playlists(),
            Event::Playlist { id } => {
                if self.current_id() == Some(id.as_str()) {
                    self.load_view();
                }
            }
            Event::Downloads { downloads } => self.downloads = downloads,
            Event::Toast { level, message } => self.toast = Some((level, message, Instant::now())),
        }
    }

    fn set_player(&mut self, p: PlayerState) {
        for id in [p.song.as_ref(), p.mixing.as_ref().map(|m| &m.song)].into_iter().flatten().map(|s| s.id.clone()).collect::<Vec<_>>() {
            self.ensure_waveform(&id);
        }
        self.player = Some(p);
        self.player_at = Instant::now();
    }

    fn ensure_waveform(&mut self, id: &str) {
        if self.waveforms.contains_key(id) || !self.waveform_requested.insert(id.to_owned()) {
            return;
        }
        let (c, id) = (self.client(), id.to_owned());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let bytes = c.waveform(&id).await.ok();
            let _ = tx.send(Msg::Waveform(id, bytes));
        });
    }

    /// Player position now, moving on between updates.
    pub fn position_ms(&self) -> f64 {
        match &self.player {
            Some(p) if p.status == PlayerStatus::Playing => p.position_ms + self.player_at.elapsed().as_secs_f64() * 1000.0 * p.rate,
            Some(p) => p.position_ms,
            None => 0.0,
        }
    }

    pub fn on_tick(&mut self) {
        if self.toast.as_ref().is_some_and(|(_, _, at)| at.elapsed() > TOAST_FOR) {
            self.toast = None;
        }
        // A running preview plays the edits as they settle.
        let due = self.editor.as_mut().and_then(|ed| ed.preview_due().map(|due| (ed.pair(), due)));
        if let Some(((from, to), (t, restart))) = due {
            let c = self.client();
            if restart {
                self.spawn(async move { c.preview(from, to, editor::LEAD_IN_MS, Some(t)).await }, Msg::Player);
            } else {
                self.spawn(async move { c.update_preview(&t).await }, Msg::Player);
            }
        }
    }

    // Input.

    pub fn on_terminal(&mut self, event: TermEvent) {
        match event {
            TermEvent::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
            TermEvent::Mouse(m) => self.on_mouse(m),
            _ => {}
        }
    }

    pub fn on_paste(&mut self, text: &str) {
        match &mut self.mode {
            Mode::Command(line) | Mode::Search { input: line, .. } => line.paste(text),
            Mode::Normal if self.editor.is_none() => {
                // Pasting a link offers to download it.
                let text = text.trim();
                if text.starts_with("http") {
                    self.mode = Mode::Command(LineInput::new(&format!("add {text}")));
                }
            }
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        self.margin = SCROLL_MARGIN;
        match std::mem::replace(&mut self.mode, Mode::Normal) {
            Mode::Command(mut line) => {
                match key.code {
                    KeyCode::Esc => self.adding = None,
                    KeyCode::Enter => self.run_command(line.text.trim().to_owned()),
                    KeyCode::Tab => {
                        line.text = complete(&line.text);
                        line.cursor = line.text.len();
                        self.mode = Mode::Command(line);
                    }
                    KeyCode::Backspace if line.text.is_empty() => {}
                    _ => {
                        line.key(key);
                        self.mode = Mode::Command(line);
                    }
                }
                return;
            }
            Mode::Search { mut input, origin } => {
                match key.code {
                    KeyCode::Esc => self.cursor = origin,
                    KeyCode::Enter => {
                        if !input.text.is_empty() {
                            self.search = Some(input.text.clone());
                        }
                    }
                    KeyCode::Backspace if input.text.is_empty() => self.cursor = origin,
                    _ => {
                        input.key(key);
                        // Jump as you type.
                        if let Some(at) = self.find(&input.text, origin, true, true) {
                            self.cursor = at;
                        }
                        self.mode = Mode::Search { input, origin };
                    }
                }
                return;
            }
            Mode::Confirm(confirm) => {
                if matches!(key.code, KeyCode::Char('y') | KeyCode::Char('Y')) {
                    self.confirmed(confirm);
                } else {
                    self.info("Cancelled");
                }
                return;
            }
            Mode::Normal => {}
        }
        if self.editor.is_some() {
            self.editor_key(key);
            return;
        }
        if self.popup.is_some() {
            self.popup_key(key);
            return;
        }
        self.normal_key(key);
    }

    fn normal_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = self.pending.trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<usize>().ok();
        let n = count.unwrap_or(1);
        let half = (self.page / 2).max(1) as isize;
        // Keys that never take part in a sequence.
        match key.code {
            KeyCode::Char('d') if ctrl => return self.reset_pending(|a| a.move_cursor(half)),
            KeyCode::Char('u') if ctrl => return self.reset_pending(|a| a.move_cursor(-half)),
            KeyCode::Char('f') if ctrl => return self.reset_pending(|a| a.move_cursor(a.page as isize)),
            KeyCode::Char('b') if ctrl => return self.reset_pending(|a| a.move_cursor(-(a.page as isize))),
            KeyCode::Char('r') if ctrl => return self.reset_pending(|a| a.playlist_action("redo")),
            KeyCode::Down => return self.reset_pending(|a| a.move_cursor(n as isize)),
            KeyCode::Up => return self.reset_pending(|a| a.move_cursor(-(n as isize))),
            KeyCode::Tab => return self.reset_pending(|a| a.toggle_focus()),
            KeyCode::Enter => return self.reset_pending(|a| a.enter()),
            KeyCode::Esc => {
                self.pending.clear();
                self.visual = None;
                return;
            }
            KeyCode::Left => return self.reset_pending(|a| a.focus = Focus::Sidebar),
            KeyCode::Right => return self.reset_pending(|a| a.focus = Focus::Main),
            _ => {}
        }
        let KeyCode::Char(c) = key.code else { return };
        if ctrl {
            self.pending.clear();
            return;
        }
        if c.is_ascii_digit() && (c != '0' || !self.pending.is_empty()) && self.pending.chars().all(|p| p.is_ascii_digit()) {
            self.pending.push(c);
            return;
        }
        self.pending.push(c);
        let ops: String = self.pending.chars().filter(|c| !c.is_ascii_digit()).collect();
        let sequence = ops.as_str();
        // In visual mode an operator applies to the selection straight away.
        let operator_waits = self.visual.is_none() || self.focus == Focus::Sidebar;
        if matches!(sequence, "g" | "z") || (matches!(sequence, "d" | "y") && operator_waits) {
            return;
        }
        self.pending.clear();
        match sequence {
            "j" => self.move_cursor(n as isize),
            "k" => self.move_cursor(-(n as isize)),
            "gg" => self.jump(count.map_or(0, |c| c.saturating_sub(1))),
            "G" => self.jump(count.map_or(usize::MAX, |c| c.saturating_sub(1))),
            "h" => self.focus = Focus::Sidebar,
            "l" => {
                if self.focus == Focus::Sidebar {
                    self.enter();
                } else {
                    self.focus = Focus::Main;
                }
            }
            "zz" => self.offset = self.cursor.saturating_sub(self.page / 2),
            "gl" => {
                self.open(View::Playlist(LIBRARY_ID.into()));
                self.focus = Focus::Main;
            }
            "gd" => {
                self.open(View::Downloads);
                self.focus = Focus::Main;
            }
            "gc" => self.go_to_playing(),
            ":" => self.mode = Mode::Command(LineInput::default()),
            "/" => self.mode = Mode::Search { input: LineInput::default(), origin: self.cursor },
            "n" => self.search_again(true),
            "N" => self.search_again(false),
            "?" => self.popup = Some(Popup::Help),
            "q" => self.quit = true,
            "Q" => {
                self.stop_daemon = true;
                self.quit = true;
            }
            "o" => self.mode = Mode::Command(LineInput::new("add ")),
            " " => self.player_action("toggle"),
            ">" => self.player_action("next"),
            "<" => self.player_action("previous"),
            "]" => self.seek_by(SEEK_MS),
            "[" => self.seek_by(-SEEK_MS),
            "}" => self.seek_by(LONG_SEEK_MS),
            "{" => self.seek_by(-LONG_SEEK_MS),
            "+" | "=" => self.change_volume(VOLUME_STEP),
            "-" => self.change_volume(-VOLUME_STEP),
            _ if self.focus == Focus::Sidebar => self.sidebar_key(sequence),
            _ if self.view == View::Downloads => self.downloads_key(sequence),
            _ => self.list_key(sequence, n),
        }
    }

    fn reset_pending(&mut self, f: impl FnOnce(&mut App)) {
        self.pending.clear();
        f(self);
    }

    fn sidebar_key(&mut self, sequence: &str) {
        if (sequence == "dd" || sequence == "x")
            && let Some((View::Playlist(id), name)) = self.side_items().get(self.side).cloned()
                && id != LIBRARY_ID
            {
                self.mode = Mode::Confirm(Confirm::DeletePlaylist(id, name));
            }
    }

    fn downloads_key(&mut self, sequence: &str) {
        if (sequence == "dd" || sequence == "x")
            && let Some(d) = self.downloads.get(self.download_cursor)
        {
            let (c, id) = (self.client(), d.id);
            self.spawn(async move { c.dismiss_download(id).await }, Msg::Downloads);
        }
    }

    fn list_key(&mut self, sequence: &str, n: usize) {
        match sequence {
            "V" => {
                self.visual = if self.visual.is_some() { None } else { Some(self.cursor) };
            }
            "t" => self.open_suggestions(),
            "e" => self.open_editor(self.cursor),
            "s" => self.playlist_action("smart-reorder"),
            "u" => self.playlist_action("undo"),
            "dd" | "x" | "d" => self.delete(n),
            "yy" | "Y" | "y" => self.yank(n),
            "p" => self.put(true),
            "P" => self.put(false),
            "J" => self.move_songs(n as isize),
            "K" => self.move_songs(-(n as isize)),
            "a" => self.open_add_to(),
            _ => {}
        }
    }

    /// The selected rows: the visual range, or `count` rows from the cursor.
    fn selection(&self, count: usize) -> (usize, usize) {
        let len = self.songs_len();
        match self.visual {
            Some(anchor) => (anchor.min(self.cursor), anchor.max(self.cursor) + 1),
            None => (self.cursor, (self.cursor + count).min(len)),
        }
    }

    fn selected_songs(&self, count: usize) -> Vec<(String, String)> {
        let (from, to) = self.selection(count);
        self.list.as_ref().map_or(Vec::new(), |l| l.songs[from.min(l.songs.len())..to.min(l.songs.len())].iter().map(|s| (s.song.id.clone(), s.song.title.clone())).collect())
    }

    pub fn move_cursor(&mut self, by: isize) {
        match (self.focus, &self.view) {
            (Focus::Sidebar, _) => self.side = step(self.side, by, self.side_items().len()),
            (Focus::Main, View::Downloads) => self.download_cursor = step(self.download_cursor, by, self.downloads.len()),
            (Focus::Main, View::Playlist(_)) => self.cursor = step(self.cursor, by, self.songs_len()),
        }
    }

    fn jump(&mut self, to: usize) {
        match (self.focus, &self.view) {
            (Focus::Sidebar, _) => self.side = to.min(self.side_items().len() - 1),
            (Focus::Main, View::Downloads) => self.download_cursor = to.min(self.downloads.len().saturating_sub(1)),
            (Focus::Main, View::Playlist(_)) => self.cursor = to.min(self.songs_len().saturating_sub(1)),
        }
    }

    fn toggle_focus(&mut self) {
        self.focus = if self.focus == Focus::Main { Focus::Sidebar } else { Focus::Main };
    }

    fn enter(&mut self) {
        match self.focus {
            Focus::Sidebar => {
                if let Some((view, _)) = self.side_items().get(self.side).cloned() {
                    self.open(view);
                    self.focus = Focus::Main;
                }
            }
            Focus::Main => match &self.view {
                View::Downloads => {
                    if let Some(id) = self.downloads.get(self.download_cursor).and_then(|d| d.song_id.clone()) {
                        self.cursor_target = Some(id);
                        self.open(View::Playlist(LIBRARY_ID.into()));
                    }
                }
                View::Playlist(id) => {
                    if self.songs_len() > 0 {
                        let (c, id, index) = (self.client(), id.clone(), self.cursor);
                        self.spawn(async move { c.play(id, index, 0.0).await }, Msg::Player);
                    }
                }
            },
        }
    }

    fn go_to_playing(&mut self) {
        let Some(p) = &self.player else { return };
        let (Some(context), Some(song)) = (p.context.clone(), p.song.as_ref().map(|s| s.id.clone())) else { return };
        let context = if context == "preview" { LIBRARY_ID.to_owned() } else { context };
        self.cursor_target = Some(song);
        self.focus = Focus::Main;
        self.open(View::Playlist(context));
    }

    fn find(&self, query: &str, from: usize, forward: bool, include_start: bool) -> Option<usize> {
        let list = self.list.as_ref()?;
        let q = query.to_lowercase();
        if q.is_empty() || list.songs.is_empty() {
            return None;
        }
        let len = list.songs.len();
        let matches = |i: usize| {
            let s = &list.songs[i].song;
            s.title.to_lowercase().contains(&q) || s.artist.to_lowercase().contains(&q)
        };
        (0..len)
            .map(|k| {
                let k = if include_start { k } else { k + 1 };
                if forward { (from + k) % len } else { (from + len * 2 - k % len) % len }
            })
            .find(|&i| matches(i))
    }

    fn search_again(&mut self, forward: bool) {
        let Some(q) = self.search.clone() else {
            self.info("No search yet; / to search");
            return;
        };
        match self.find(&q, self.cursor, forward, false) {
            Some(at) => self.cursor = at,
            None => self.error(format!("Not found: {q}")),
        }
    }

    // Player.

    fn player_action(&self, action: &'static str) {
        let c = self.client();
        self.spawn(async move { c.player_action(action).await }, Msg::Player);
    }

    fn seek_by(&self, ms: f64) {
        if self.player.as_ref().is_none_or(|p| p.song.is_none()) {
            return;
        }
        let (c, to) = (self.client(), (self.position_ms() + ms).max(0.0));
        self.spawn(async move { c.seek(to).await }, Msg::Player);
    }

    fn change_volume(&mut self, by: i16) {
        let now = self.player.as_ref().map_or(80, |p| i16::from(p.volume));
        let volume = (now + by).clamp(0, 100) as u8;
        if let Some(p) = &mut self.player {
            p.volume = volume;
        }
        let c = self.client();
        self.spawn(async move { c.volume(volume).await }, Msg::Player);
    }

    // Playlist edits.

    fn playlist_action(&mut self, action: &'static str) {
        let Some(id) = self.current_id().map(str::to_owned) else { return };
        if self.songs_len() == 0 {
            return;
        }
        if action == "smart-reorder" {
            self.info("Reordering… u to undo");
        }
        let c = self.client();
        self.spawn(async move { c.playlist_action(&id, action).await }, Msg::View);
    }

    fn delete(&mut self, count: usize) {
        let Some(id) = self.current_id().map(str::to_owned) else { return };
        let songs = self.selected_songs(count);
        if songs.is_empty() {
            return;
        }
        let (from, to) = self.selection(count);
        self.visual = None;
        if id == LIBRARY_ID {
            // Deleting from the library deletes the files, so ask first.
            self.mode = Mode::Confirm(Confirm::DeleteSongs(songs));
            return;
        }
        self.register = Some(Register { songs: songs.into_iter().map(|(id, _)| id).collect() });
        self.cursor = from;
        let c = self.client();
        self.spawn(async move { c.remove_songs(&id, (from..to).collect()).await }, Msg::View);
    }

    fn yank(&mut self, count: usize) {
        let songs: Vec<String> = self.selected_songs(count).into_iter().map(|(id, _)| id).collect();
        if songs.is_empty() {
            return;
        }
        self.info(format!("Yanked {}", crate::theme::plural(songs.len(), "song")));
        self.register = Some(Register { songs });
        self.visual = None;
    }

    fn put(&mut self, after: bool) {
        let Some(id) = self.current_id().map(str::to_owned) else { return };
        let Some(register) = self.register.clone() else {
            self.info("Nothing yanked; yy or dd first");
            return;
        };
        let at = if self.songs_len() == 0 { 0 } else if after { self.cursor + 1 } else { self.cursor };
        self.cursor_target = register.songs.first().cloned();
        let c = self.client();
        self.spawn(async move { c.add_songs(&id, register.songs, Some(at)).await }, Msg::View);
    }

    fn move_songs(&mut self, by: isize) {
        let Some(id) = self.current_id().map(str::to_owned) else { return };
        let (from, to) = self.selection(1);
        let count = to - from;
        let len = self.songs_len();
        if count == 0 {
            return;
        }
        let target = (from as isize + by).clamp(0, (len - count) as isize) as usize;
        if target == from {
            return;
        }
        let shift = target as isize - from as isize;
        self.cursor = (self.cursor as isize + shift) as usize;
        if let Some(anchor) = self.visual {
            self.visual = Some((anchor as isize + shift) as usize);
        }
        let c = self.client();
        self.spawn(async move { c.move_songs(&id, from, count, target).await }, Msg::View);
    }

    fn confirmed(&mut self, confirm: Confirm) {
        match confirm {
            Confirm::DeleteSongs(songs) => {
                let c = self.client();
                let n = songs.len();
                self.spawn(
                    async move {
                        for (id, _) in songs {
                            c.delete_song(&id).await?;
                        }
                        Ok(())
                    },
                    move |()| Msg::Info(format!("Deleted {}", crate::theme::plural(n, "song"))),
                );
            }
            Confirm::DeletePlaylist(id, name) => {
                let c = self.client();
                self.spawn(async move { c.delete_playlist(&id).await }, move |()| Msg::Info(format!("Deleted playlist {name}")));
            }
        }
    }

    // Popups.

    fn open_suggestions(&mut self) {
        let (Some(id), Some(list)) = (self.current_id().map(str::to_owned), &self.list) else { return };
        let Some(anchor) = list.songs.get(self.cursor) else { return };
        let popup = SuggestionsPopup { playlist: id.clone(), anchor: self.cursor, anchor_title: anchor.song.title.clone(), items: None, cursor: 0 };
        self.popup = Some(Popup::Suggestions(popup));
        let (c, index, playlist) = (self.client(), self.cursor, id.clone());
        self.spawn(async move { c.suggestions(&id, index).await }, move |list| Msg::Suggestions { playlist, anchor: index, list });
    }

    fn open_add_to(&mut self) {
        let songs: Vec<String> = self.selected_songs(1).into_iter().map(|(id, _)| id).collect();
        if songs.is_empty() {
            return;
        }
        self.visual = None;
        self.popup = Some(Popup::AddTo(AddToPopup { songs, cursor: 0 }));
    }

    /// Playlists offered by the "add to" popup, then "new playlist".
    pub fn add_targets(&self) -> Vec<(Option<String>, String)> {
        let mut items: Vec<(Option<String>, String)> = self
            .playlists
            .as_ref()
            .map(|p| p.playlists.iter().filter(|p| Some(p.id.as_str()) != self.current_id()).map(|p| (Some(p.id.clone()), p.name.clone())).collect())
            .unwrap_or_default();
        items.push((None, "New playlist…".into()));
        items
    }

    fn popup_key(&mut self, key: KeyEvent) {
        let Some(popup) = self.popup.take() else { return };
        let close = matches!(key.code, KeyCode::Esc | KeyCode::Char('q'));
        match popup {
            Popup::Help => {}
            Popup::Suggestions(mut p) => {
                let len = p.items.as_ref().map_or(0, Vec::len);
                match key.code {
                    _ if close => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        p.cursor = (p.cursor + 1).min(len.saturating_sub(1));
                        self.popup = Some(Popup::Suggestions(p));
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        p.cursor = p.cursor.saturating_sub(1);
                        self.popup = Some(Popup::Suggestions(p));
                    }
                    KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('1'..='5') => {
                        let pick = match key.code {
                            KeyCode::Char(c @ '1'..='5') => c as usize - '1' as usize,
                            _ => p.cursor,
                        };
                        if !self.pick_suggestion(&p, pick) {
                            self.popup = Some(Popup::Suggestions(p));
                        }
                    }
                    _ => self.popup = Some(Popup::Suggestions(p)),
                }
            }
            Popup::AddTo(mut p) => {
                let targets = self.add_targets();
                match key.code {
                    _ if close => {}
                    KeyCode::Char('j') | KeyCode::Down => {
                        p.cursor = (p.cursor + 1).min(targets.len() - 1);
                        self.popup = Some(Popup::AddTo(p));
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        p.cursor = p.cursor.saturating_sub(1);
                        self.popup = Some(Popup::AddTo(p));
                    }
                    KeyCode::Enter | KeyCode::Char('l') => {
                        let at = p.cursor;
                        self.add_to(p, at);
                    }
                    _ => self.popup = Some(Popup::AddTo(p)),
                }
            }
        }
    }

    /// Puts suggestion `pick` after the song it was asked for; false if
    /// there is no such suggestion.
    fn pick_suggestion(&mut self, p: &SuggestionsPopup, pick: usize) -> bool {
        let Some(s) = p.items.as_ref().and_then(|items| items.get(pick)) else { return false };
        let (c, id, anchor, song) = (self.client(), p.playlist.clone(), p.anchor, s.song.song.id.clone());
        self.cursor_target = Some(song.clone());
        self.info(format!("{} now plays after {}", s.song.song.title, p.anchor_title));
        self.spawn(async move { c.place_after(&id, anchor, song).await }, Msg::View);
        true
    }

    /// Adds the popup's songs to the playlist at `at` in `add_targets`.
    fn add_to(&mut self, p: AddToPopup, at: usize) {
        match self.add_targets().get(at) {
            Some((Some(id), name)) => {
                let (c, id, songs) = (self.client(), id.clone(), p.songs);
                let message = format!("Added {} to {name}", crate::theme::plural(songs.len(), "song"));
                self.spawn(async move { c.add_songs(&id, songs, None).await }, move |_| Msg::Info(message));
            }
            Some((None, _)) => {
                self.adding = Some(p.songs);
                self.mode = Mode::Command(LineInput::new("new "));
                self.info("Name the playlist; the selection goes in it");
            }
            None => {}
        }
    }

    // Transition editor.

    fn open_editor(&mut self, index: usize) {
        let Some(list) = &self.list else { return };
        if index + 1 >= list.songs.len() {
            self.info("The last song has no transition after it");
            return;
        }
        let (from, to) = (list.songs[index].song.id.clone(), list.songs[index + 1].song.id.clone());
        let mut editor = Editor::new(index, list.songs.len(), from.clone(), to.clone());
        if let Some(e) = &self.editor {
            editor.previewing = e.previewing;
            editor.pick_field(e.field());
        }
        self.ensure_waveform(&from);
        self.ensure_waveform(&to);
        let previewing = editor.previewing;
        self.editor = Some(editor);
        let c = self.client();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            match c.transition(&from, &to).await {
                Ok(d) => {
                    let _ = tx.send(Msg::Transition(d));
                    if previewing && let Ok(p) = c.preview(from, to, editor::LEAD_IN_MS, None).await {
                        let _ = tx.send(Msg::Player(p));
                    }
                }
                Err(e) => {
                    let _ = tx.send(Msg::Error(format!("{e:#}")));
                }
            }
        });
    }

    /// `:w` stores the draft; `close` then shuts the editor.
    fn write_transition(&mut self, close: bool) {
        let Some(ed) = &self.editor else { return };
        let Some(t) = ed.draft.clone() else {
            self.error("The transition hasn't loaded yet");
            return;
        };
        let ((from, to), c, sent) = (ed.pair(), self.client(), t.clone());
        self.spawn(
            async move {
                // An automatic one is stored by forgetting the edited one.
                if t.auto { c.reset_transition(&from, &to).await } else { c.set_transition(&from, &to, &t).await }
            },
            move |detail| Msg::Saved { detail, sent, close },
        );
    }

    /// Shuts the editor, unless that would lose edits and `force` isn't given.
    fn quit_editor(&mut self, force: bool) {
        if !force && self.editor.as_ref().is_some_and(Editor::modified) {
            self.error("No write since last change (:w saves, :q! throws the edits away)");
            return;
        }
        self.close_editor();
    }

    fn close_editor(&mut self) {
        if self.editor.as_ref().is_some_and(|e| e.previewing) {
            self.player_action("stop");
        }
        self.editor = None;
        self.load_view();
    }

    fn toggle_preview(&mut self) {
        let Some(ed) = self.editor.as_mut() else { return };
        if ed.previewing {
            ed.previewing = false;
            self.player_action("stop");
            return;
        }
        let ((from, to), Some(t)) = (ed.pair(), ed.start_preview()) else { return };
        let c = self.client();
        self.spawn(async move { c.preview(from, to, editor::LEAD_IN_MS, Some(t)).await }, Msg::Player);
    }

    fn editor_key(&mut self, key: KeyEvent) {
        let Some(action) = self.editor.as_mut().map(|ed| ed.key(key)) else { return };
        match action {
            Action::None => {}
            Action::Close => self.quit_editor(false),
            Action::Command => self.mode = Mode::Command(LineInput::default()),
            Action::TogglePreview => self.toggle_preview(),
            Action::Error(e) => self.error(e),
            Action::Pair(by) => {
                let Some(ed) = self.editor.as_ref() else { return };
                if ed.modified() {
                    self.error("No write since last change (:w saves, :e! throws the edits away)");
                    return;
                }
                let target = ed.index as isize + by;
                if target >= 0 && (target as usize) + 1 < ed.count {
                    self.cursor = target as usize;
                    self.open_editor(target as usize);
                }
            }
        }
    }

    /// Commands that act on the open transition editor. False for the rest.
    fn editor_command(&mut self, name: &str) -> bool {
        match name {
            "w" | "w!" | "write" => self.write_transition(false),
            "wq" | "wq!" | "x" | "x!" | "xit" => self.write_transition(true),
            "q" | "quit" => self.quit_editor(false),
            "q!" | "quit!" => self.quit_editor(true),
            "e!" | "edit!" => {
                if let Some(ed) = &mut self.editor {
                    ed.revert();
                }
            }
            "qa" if self.editor.as_ref().is_some_and(Editor::modified) => self.error("No write since last change (:w saves, :qa! quits anyway)"),
            _ => return false,
        }
        true
    }

    // Mouse.

    fn on_mouse(&mut self, m: MouseEvent) {
        // The command line and questions want the keyboard.
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let hit = self.hits.at(m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pending.clear();
                self.drag = None;
                if let Some((area, target)) = hit {
                    let double = self.clicks.click(target);
                    self.click(target, area, m.column, m.row, double);
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => self.drag_to(m.column, m.row),
            MouseEventKind::Up(MouseButton::Left) => self.drag = None,
            MouseEventKind::ScrollDown => {
                if let Some((_, target)) = hit {
                    self.scroll(target, 1);
                }
            }
            MouseEventKind::ScrollUp => {
                if let Some((_, target)) = hit {
                    self.scroll(target, -1);
                }
            }
            _ => {}
        }
    }

    /// A left click on `target`, drawn over `area`.
    fn click(&mut self, target: Target, area: Rect, column: u16, row: u16, double: bool) {
        match target {
            Target::Backdrop => self.popup = None,
            Target::Popup | Target::Volume => {}
            Target::Pane(focus) => self.focus = focus,
            Target::Side(i) => {
                self.side = i;
                self.focus = Focus::Sidebar;
                self.enter();
            }
            Target::Song(i) => {
                self.focus = Focus::Main;
                self.margin = 0;
                self.visual = None;
                self.cursor = i;
                self.drag = Some(Drag::Select { anchor: i });
                if double {
                    self.enter();
                }
            }
            Target::Link(i) => {
                self.focus = Focus::Main;
                self.margin = 0;
                self.visual = None;
                self.cursor = i;
                self.open_editor(i);
            }
            Target::Download(i) => {
                self.focus = Focus::Main;
                self.download_cursor = i;
                if double {
                    self.enter();
                }
            }
            Target::Seek => {
                let Some(song) = self.player.as_ref().and_then(|p| p.song.as_ref()) else { return };
                let to = song.duration_ms as f64 * f64::from(column - area.x) / f64::from(area.width);
                let c = self.client();
                self.spawn(async move { c.seek(to).await }, Msg::Player);
            }
            Target::Toggle => self.player_action("toggle"),
            // Leaving the editor this way would leave it on another list.
            Target::Playing if self.editor.is_none() => self.go_to_playing(),
            Target::Playing => {}
            Target::Suggestion(i) => {
                let Some(Popup::Suggestions(mut p)) = self.popup.take() else { return };
                p.cursor = i;
                if !(double && self.pick_suggestion(&p, i)) {
                    self.popup = Some(Popup::Suggestions(p));
                }
            }
            Target::AddTo(i) => {
                let Some(Popup::AddTo(p)) = self.popup.take() else { return };
                self.add_to(p, i);
            }
            Target::Field(_) | Target::Step(..) | Target::Lane(..) | Target::Curve { .. } | Target::Preset(_) | Target::Automatic => {
                let Some(ed) = self.editor.as_mut() else { return };
                match target {
                    Target::Field(f) => {
                        ed.pick_field(f);
                        if double && let Action::Error(e) = ed.customize() {
                            self.error(e);
                        }
                    }
                    Target::Step(f, by) => {
                        ed.pick_field(f);
                        ed.change(f, by, false);
                    }
                    Target::Lane(f, ms_per_column) => {
                        ed.pick_field(f);
                        if let Some(from_ms) = ed.cue(f) {
                            self.drag = Some(Drag::Cue { field: f, column, from_ms, ms_per_column });
                        }
                    }
                    Target::Curve { track, t0, ms_per_col, duration_ms } => {
                        let (x, _) = curve_point(area, t0, ms_per_col, duration_ms, column, row);
                        // Within a column and a half of a point picks it.
                        let reach = (1.5 * ms_per_col / duration_ms) as f32;
                        if ed.click_curve(track, x, reach, double) {
                            self.drag = Some(Drag::Point { area, t0, ms_per_col, duration_ms });
                        }
                    }
                    Target::Preset(i) => ed.preset(Preset::ALL[i]),
                    Target::Automatic => ed.reset(),
                    _ => {}
                }
            }
        }
    }

    fn drag_to(&mut self, column: u16, row: u16) {
        match self.drag {
            Some(Drag::Select { anchor }) => {
                // Above or below the list, it scrolls a row at a time.
                let a = self.list_area;
                let to = if row < a.y {
                    self.cursor.saturating_sub(1)
                } else if row >= a.bottom() {
                    self.cursor + 1
                } else {
                    self.offset + usize::from(row - a.y)
                };
                let to = to.min(self.songs_len().saturating_sub(1));
                if to != anchor {
                    self.visual = Some(anchor);
                }
                self.cursor = to;
            }
            Some(Drag::Cue { field, column: from, from_ms, ms_per_column }) => {
                if let Some(ed) = self.editor.as_mut() {
                    // The waveform follows the pointer, so dragging right
                    // brings an earlier part of the song into the window.
                    ed.drag(field, from_ms, -(f64::from(column) - f64::from(from)) * ms_per_column);
                }
            }
            Some(Drag::Point { area, t0, ms_per_col, duration_ms }) => {
                if let Some(ed) = self.editor.as_mut() {
                    let (x, y) = curve_point(area, t0, ms_per_col, duration_ms, column, row);
                    // Half a column either side of a beat lands on it.
                    ed.drag_point(x, y, (0.5 * ms_per_col / duration_ms) as f32);
                }
            }
            None => {}
        }
    }

    /// The wheel over `target`; `down` is 1 or -1.
    fn scroll(&mut self, target: Target, down: isize) {
        match target {
            Target::Song(_) | Target::Link(_) | Target::Pane(Focus::Main) if matches!(self.view, View::Playlist(_)) => {
                self.scroll_list(down * SCROLL_LINES);
            }
            Target::Suggestion(_) | Target::AddTo(_) | Target::Popup => {
                let targets = self.add_targets().len();
                match &mut self.popup {
                    Some(Popup::Suggestions(p)) => p.cursor = step(p.cursor, down, p.items.as_ref().map_or(0, Vec::len)),
                    Some(Popup::AddTo(p)) => p.cursor = step(p.cursor, down, targets),
                    _ => {}
                }
            }
            Target::Volume => self.change_volume(-down as i16 * VOLUME_STEP),
            // Up raises a value, as on a knob.
            Target::Field(f) | Target::Step(f, _) | Target::Lane(f, _) => {
                if let Some(ed) = self.editor.as_mut() {
                    ed.pick_field(f);
                    ed.change(f, -down as i32, false);
                }
            }
            _ => {}
        }
    }

    /// Scrolls the song list, taking the cursor along when it would go off screen.
    fn scroll_list(&mut self, by: isize) {
        let len = self.songs_len();
        self.offset = step(self.offset, by, len.saturating_sub(self.page) + 1);
        let last = (self.offset + self.page).min(len).saturating_sub(1);
        self.cursor = self.cursor.clamp(self.offset, last.max(self.offset));
        self.margin = 0;
    }

    // Commands.

    fn run_command(&mut self, line: String) {
        let (name, arg) = line.split_once(' ').map_or((line.as_str(), ""), |(n, a)| (n, a.trim()));
        let arg = arg.to_owned();
        if let Ok(n) = name.parse::<usize>() {
            self.jump(n.saturating_sub(1));
            return;
        }
        if self.editor.is_some() && self.editor_command(name) {
            return;
        }
        let c = self.client();
        match name {
            "" => {}
            "q" | "quit" | "wq" | "x" => self.quit = true,
            "w" | "write" => self.info("Playlists save as you go; :w saves a transition you are editing (e)"),
            "q!" | "qa" | "qa!" => {
                self.stop_daemon = true;
                self.quit = true;
            }
            "add" | "download" | "dl" => {
                if arg.is_empty() {
                    self.error("Usage: :add <link>");
                    return;
                }
                self.info("Queued; gd shows downloads");
                self.spawn(async move { c.download(arg).await }, Msg::Downloads);
            }
            "new" => {
                if arg.is_empty() {
                    self.error("Usage: :new <name>");
                    return;
                }
                let songs = self.adding.take().unwrap_or_default();
                self.spawn(async move { c.create_playlist(arg, Vec::new()).await }, move |p| Msg::Created(p, songs));
            }
            "rename" => {
                let Some(id) = self.current_id().map(str::to_owned).filter(|id| id != LIBRARY_ID) else {
                    self.error("Open a playlist to rename it");
                    return;
                };
                let update = PlaylistUpdate { name: Some(arg) };
                self.spawn(async move { c.update_playlist(&id, update).await }, Msg::View);
            }
            "delete" => {
                if let Some((View::Playlist(id), name)) = self.side_items().into_iter().find(|(v, _)| *v == self.view)
                    && id != LIBRARY_ID
                {
                    self.mode = Mode::Confirm(Confirm::DeletePlaylist(id, name));
                } else {
                    self.error("Open a playlist to delete it");
                }
            }
            "reorder" | "smart" => self.playlist_action("smart-reorder"),
            "undo" => self.playlist_action("undo"),
            "redo" => self.playlist_action("redo"),
            "title" | "artist" => {
                let Some(song) = self.list.as_ref().and_then(|l| l.songs.get(self.cursor)).map(|s| s.song.id.clone()) else { return };
                let update = if name == "title" { SongUpdate { title: Some(arg), artist: None } } else { SongUpdate { title: None, artist: Some(arg) } };
                self.spawn(async move { c.update_song(&song, update).await }, |s| Msg::Info(format!("Renamed to {} — {}", s.song.artist, s.song.title)));
            }
            "vol" | "volume" => match arg.parse::<u8>() {
                Ok(v) => self.spawn(async move { c.volume(v.min(100)).await }, Msg::Player),
                Err(_) => self.error("Usage: :vol 0-100"),
            },
            "seek" => match parse_clock(&arg) {
                Some(ms) => self.spawn(async move { c.seek(ms).await }, Msg::Player),
                None => self.error("Usage: :seek 1:30"),
            },
            "stop" => self.player_action("stop"),
            "cookies" => {
                if arg.is_empty() {
                    let now = self.status.as_ref().and_then(|s| s.cookies_from_browser.clone());
                    self.info(match now {
                        Some(b) => format!("Downloads use {b}'s YouTube login; :cookies off to stop"),
                        None => "Usage: :cookies firefox (or chrome, brave, edge, safari…) · :cookies off".into(),
                    });
                    return;
                }
                let browser = if arg == "off" { String::new() } else { arg };
                // The new status also arrives as an event.
                self.spawn(async move { c.update_config(ConfigUpdate { cookies_from_browser: Some(browser) }).await }, |s| {
                    Msg::Info(match &s.cookies_from_browser {
                        Some(b) => format!("Downloads now use {b}'s YouTube login; retry with :add"),
                        None => "Downloads no longer use a browser login".into(),
                    })
                });
            }
            "rescan" => self.spawn(async move { c.rescan().await }, |n| Msg::Info(format!("Found {}", crate::theme::plural(n, "new song")))),
            "clear" => self.spawn(async move { c.clear_downloads().await }, Msg::Downloads),
            "help" | "h" => self.popup = Some(Popup::Help),
            other => self.error(format!("Not a command: {other} (:help lists them)")),
        }
    }
}

/// Commands `Tab` completes.
pub const COMMANDS: [&str; 24] = [
    "add", "artist", "clear", "cookies", "delete", "download", "help", "new", "q", "q!", "quit", "redo", "rename",
    "reorder", "rescan", "seek", "smart", "stop", "title", "undo", "vol", "w", "wq", "x",
];

/// Where a click at (`column`, `row`) lands on a curve drawn over `area`:
/// how far through the window, and the value there.
fn curve_point(area: Rect, t0: f64, ms_per_col: f64, duration_ms: f64, column: u16, row: u16) -> (f32, f32) {
    let across = f64::from(column.saturating_sub(area.x)) + 0.5;
    let x = (t0 + across * ms_per_col) / duration_ms;
    let down = f64::from(row.saturating_sub(area.y)) / f64::from(area.height.saturating_sub(1).max(1));
    (x.clamp(0.0, 1.0) as f32, (1.0 - down).clamp(0.0, 1.0) as f32)
}

/// `at` moved `by` within `0..len`.
fn step(at: usize, by: isize, len: usize) -> usize {
    (at as isize + by).clamp(0, (len as isize - 1).max(0)) as usize
}

fn complete(line: &str) -> String {
    if line.contains(' ') {
        return line.to_owned();
    }
    let matching: Vec<&str> = COMMANDS.iter().copied().filter(|c| c.starts_with(line)).collect();
    match matching.as_slice() {
        [one] => format!("{one} "),
        _ => line.to_owned(),
    }
}

/// `1:30`, `90`, `1:02:03` as milliseconds.
fn parse_clock(s: &str) -> Option<f64> {
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(total * 1000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_a_line() {
        let mut l = LineInput::new("add http");
        let key = |c| KeyEvent::new(c, KeyModifiers::NONE);
        l.key(key(KeyCode::Left));
        l.key(key(KeyCode::Char('X')));
        assert_eq!(l.text, "add httXp");
        l.key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(l.text, "add p");
        l.paste("é\n");
        assert_eq!(l.text, "add ép");
    }

    #[test]
    fn completes_commands_and_parses_times() {
        assert_eq!(complete("reo"), "reorder ");
        assert_eq!(complete("re"), "re");
        assert_eq!(parse_clock("1:30"), Some(90_000.0));
        assert_eq!(parse_clock("45"), Some(45_000.0));
        assert_eq!(parse_clock("x"), None);
    }
}
