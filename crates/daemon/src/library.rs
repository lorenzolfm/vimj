//! Songs, playlists and hand-edited transitions, kept in `library.json`.
//!
//! The library itself is a playlist too (`LIBRARY_ID`): its order is the
//! order songs are stored in, and it can be reordered and mixed like any
//! other. Transitions belong to a pair of songs, so a pair tuned once sounds
//! the same wherever it occurs; pairs nobody edited get an automatic one.

use crate::config::{load_json, save_json, write_atomic};
use anyhow::Context;
use vimj_audio::engine::{Entry, normalising_gain};
use vimj_core::compat::{Compat, compat};
use vimj_core::order;
use vimj_core::protocol::*;
use vimj_core::transition::{Side, auto_transition};
use vimj_core::{Analysis, Song, Transition};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Undo steps kept per playlist.
const HISTORY: usize = 50;
/// The format of `library.json`. Bumped whenever this build writes what an
/// older one can't read, so that an older one refuses the file instead.
const STORE_VERSION: u32 = 1;

#[derive(Debug)]
pub enum Error {
    NotFound(String),
    Invalid(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound(what) => write!(f, "{what} not found"),
            Error::Invalid(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

fn not_found(what: impl Into<String>) -> Error {
    Error::NotFound(what.into())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Playlist {
    pub id: String,
    pub name: String,
    pub songs: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Store {
    /// 0 for files from before the format had a number.
    #[serde(default)]
    version: u32,
    #[serde(default)]
    songs: Vec<Song>,
    #[serde(default)]
    playlists: Vec<Playlist>,
    /// Hand-edited transitions, keyed `"{from}>{to}"`.
    #[serde(default)]
    transitions: HashMap<String, Transition>,
    #[serde(default)]
    next_playlist: u64,
}

impl Default for Store {
    fn default() -> Store {
        Store { version: STORE_VERSION, songs: Vec::new(), playlists: Vec::new(), transitions: HashMap::new(), next_playlist: 1 }
    }
}

#[derive(Default)]
struct History {
    undo: Vec<Vec<String>>,
    redo: Vec<Vec<String>>,
}

pub struct Library {
    path: PathBuf,
    store: Store,
    states: HashMap<String, SongState>,
    history: HashMap<String, History>,
}

fn pair_key(from: &str, to: &str) -> String {
    format!("{from}>{to}")
}

/// Copies a library in an older format aside before this build saves its own
/// over it: older builds can't read that, and start over empty. The first
/// copy of each format is the one kept.
fn keep_for_older_builds(path: &Path, version: u32) -> anyhow::Result<()> {
    let copy = path.with_extension(format!("v{version}.json"));
    if !copy.exists() {
        let keep = || write_atomic(&copy, &std::fs::read(path)?);
        keep().with_context(|| format!("copying {} to {}", path.display(), copy.display()))?;
        tracing::info!("kept the library as older vimj builds read it in {}", copy.display());
    }
    Ok(())
}

impl Library {
    /// Reads the library, or starts an empty one where there is none yet. A
    /// library this build can't read is an error, so that nothing saves over it.
    pub fn load(path: PathBuf) -> anyhow::Result<Library> {
        /// Read first, as a newer format may not parse as this one.
        #[derive(Deserialize)]
        struct Format {
            #[serde(default)]
            version: u32,
        }
        if let Some(Format { version }) = load_json(&path)?
            && version > STORE_VERSION
        {
            anyhow::bail!("{} is from a newer vimj (format {version}; this one reads up to {STORE_VERSION})", path.display());
        }
        let mut store: Store = load_json(&path)?.unwrap_or_default();
        if store.version < STORE_VERSION {
            keep_for_older_builds(&path, store.version)?;
            store.version = STORE_VERSION;
        }
        Ok(Library { path, store, states: HashMap::new(), history: HashMap::new() })
    }

    #[cfg(test)]
    fn in_memory() -> Library {
        Library { path: PathBuf::new(), store: Store::default(), states: HashMap::new(), history: HashMap::new() }
    }

    pub fn save(&self) {
        if self.path.as_os_str().is_empty() {
            return;
        }
        if let Err(e) = save_json(&self.path, &self.store) {
            tracing::error!("saving {}: {e:#}", self.path.display());
        }
    }

    // Songs.

    pub fn songs(&self) -> &[Song] {
        &self.store.songs
    }

    pub fn song(&self, id: &str) -> Option<&Song> {
        self.store.songs.iter().find(|s| s.id == id)
    }

    fn song_mut(&mut self, id: &str) -> Result<&mut Song> {
        self.store.songs.iter_mut().find(|s| s.id == id).ok_or_else(|| not_found("song"))
    }

    pub fn state(&self, id: &str) -> SongState {
        self.states.get(id).copied().unwrap_or_else(|| match self.song(id) {
            Some(s) if s.analysis.is_some() => SongState::Ready,
            _ => SongState::Analyzing,
        })
    }

    pub fn set_state(&mut self, id: &str, state: SongState) {
        self.states.insert(id.to_owned(), state);
    }

    pub fn info(&self, id: &str) -> Result<SongInfo> {
        let song = self.song(id).ok_or_else(|| not_found("song"))?;
        Ok(SongInfo { song: song.clone(), state: self.state(id) })
    }

    pub fn has_file(&self, file: &str) -> bool {
        self.store.songs.iter().any(|s| s.file == file)
    }

    pub fn add_song(&mut self, song: Song) {
        self.store.songs.retain(|s| s.id != song.id);
        self.store.songs.push(song);
        self.save();
    }

    /// Takes a song out of the library, its playlists and its transitions.
    pub fn remove_song(&mut self, id: &str) -> Result<Song> {
        let at = self.store.songs.iter().position(|s| s.id == id).ok_or_else(|| not_found("song"))?;
        let song = self.store.songs.remove(at);
        for p in &mut self.store.playlists {
            p.songs.retain(|s| s != id);
        }
        self.store.transitions.retain(|k, _| k.split_once('>').is_none_or(|(a, b)| a != id && b != id));
        self.states.remove(id);
        self.history.clear();
        self.save();
        Ok(song)
    }

    pub fn update_song(&mut self, id: &str, update: SongUpdate) -> Result<SongInfo> {
        let song = self.song_mut(id)?;
        if let Some(title) = update.title.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()) {
            song.title = title;
        }
        if let Some(artist) = update.artist {
            song.artist = artist.trim().to_owned();
        }
        self.save();
        self.info(id)
    }

    pub fn set_analysis(&mut self, id: &str, analysis: Analysis, duration_ms: u64) -> Result<()> {
        let song = self.song_mut(id)?;
        song.analysis = Some(analysis);
        song.duration_ms = duration_ms;
        self.states.insert(id.to_owned(), SongState::Ready);
        self.save();
        Ok(())
    }

    /// Next song still waiting for analysis.
    pub fn needs_analysis(&self, version: u32) -> Option<Song> {
        self.store
            .songs
            .iter()
            .find(|s| {
                s.analysis.as_ref().is_none_or(|a| a.version < version)
                    && !matches!(self.states.get(&s.id), Some(SongState::Failed | SongState::Missing))
            })
            .cloned()
    }

    // Playlists.

    pub fn playlists(&self) -> Playlists {
        let summary = |id: &str, name: &str, ids: &[String]| PlaylistSummary {
            id: id.to_owned(),
            name: name.to_owned(),
            count: ids.len(),
            duration_ms: ids.iter().filter_map(|id| self.song(id)).map(|s| s.duration_ms).sum(),
        };
        let all: Vec<String> = self.store.songs.iter().map(|s| s.id.clone()).collect();
        Playlists {
            library: summary(LIBRARY_ID, "Library", &all),
            playlists: self.store.playlists.iter().map(|p| summary(&p.id, &p.name, &p.songs)).collect(),
        }
    }

    fn playlist(&self, id: &str) -> Result<&Playlist> {
        self.store.playlists.iter().find(|p| p.id == id).ok_or_else(|| not_found("playlist"))
    }

    fn playlist_mut(&mut self, id: &str) -> Result<&mut Playlist> {
        self.store.playlists.iter_mut().find(|p| p.id == id).ok_or_else(|| not_found("playlist"))
    }

    pub fn order(&self, id: &str) -> Result<Vec<String>> {
        if id == LIBRARY_ID {
            return Ok(self.store.songs.iter().map(|s| s.id.clone()).collect());
        }
        Ok(self.playlist(id)?.songs.clone())
    }

    /// Replaces a playlist's order, recording the old one for undo.
    fn set_order(&mut self, id: &str, new: Vec<String>) -> Result<()> {
        let old = self.order(id)?;
        if old == new {
            return Ok(());
        }
        let history = self.history.entry(id.to_owned()).or_default();
        history.undo.push(old);
        if history.undo.len() > HISTORY {
            history.undo.remove(0);
        }
        history.redo.clear();
        self.write_order(id, new)
    }

    fn write_order(&mut self, id: &str, new: Vec<String>) -> Result<()> {
        if id == LIBRARY_ID {
            let mut by_id: HashMap<String, Song> = self.store.songs.drain(..).map(|s| (s.id.clone(), s)).collect();
            self.store.songs = new.iter().filter_map(|i| by_id.remove(i)).collect();
            // Anything the new order left out keeps its place at the end.
            let mut rest: Vec<Song> = by_id.into_values().collect();
            rest.sort_by_key(|s| s.added);
            self.store.songs.extend(rest);
        } else {
            let known: HashSet<&str> = self.store.songs.iter().map(|s| s.id.as_str()).collect();
            let songs: Vec<String> = new.into_iter().filter(|s| known.contains(s.as_str())).collect();
            self.playlist_mut(id)?.songs = songs;
        }
        self.save();
        Ok(())
    }

    pub fn create_playlist(&mut self, name: &str, songs: Vec<String>) -> Result<PlaylistSummary> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("a playlist needs a name".into()));
        }
        let id = format!("p{}", self.store.next_playlist);
        self.store.next_playlist += 1;
        let mut seen = HashSet::new();
        let songs = songs.into_iter().filter(|s| self.song(s).is_some() && seen.insert(s.clone())).collect();
        self.store.playlists.push(Playlist { id: id.clone(), name: name.to_owned(), songs });
        self.save();
        Ok(self.playlists().playlists.into_iter().find(|p| p.id == id).expect("just added"))
    }

    pub fn update_playlist(&mut self, id: &str, update: PlaylistUpdate) -> Result<()> {
        if id == LIBRARY_ID {
            if update.name.is_some() {
                return Err(Error::Invalid("the library can't be renamed".into()));
            }
        } else {
            let p = self.playlist_mut(id)?;
            if let Some(name) = update.name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()) {
                p.name = name;
            }
        }
        self.save();
        Ok(())
    }

    pub fn delete_playlist(&mut self, id: &str) -> Result<()> {
        if id == LIBRARY_ID {
            return Err(Error::Invalid("the library can't be deleted".into()));
        }
        self.playlist(id)?;
        self.store.playlists.retain(|p| p.id != id);
        self.history.remove(id);
        self.save();
        Ok(())
    }

    pub fn view(&self, id: &str) -> Result<PlaylistView> {
        let ids = self.order(id)?;
        let name = if id == LIBRARY_ID { "Library".to_owned() } else { self.playlist(id)?.name.clone() };
        let songs: Vec<SongInfo> = ids.iter().filter_map(|i| self.info(i).ok()).collect();
        let links = songs.windows(2).map(|w| self.link(&w[0].song.id, &w[1].song.id)).collect();
        let history = self.history.get(id);
        Ok(PlaylistView {
            id: id.to_owned(),
            name,
            songs,
            links,
            can_undo: history.is_some_and(|h| !h.undo.is_empty()),
            can_redo: history.is_some_and(|h| !h.redo.is_empty()),
        })
    }

    /// Adds songs at `at` (the end when `None`). Songs already in the playlist
    /// move there instead, which is all putting songs into the library does.
    pub fn add_songs(&mut self, id: &str, songs: &[String], at: Option<usize>) -> Result<()> {
        let mut order = self.order(id)?;
        let adding: Vec<String> = songs.iter().filter(|s| self.song(s).is_some()).cloned().collect();
        if adding.is_empty() {
            return Err(not_found("song"));
        }
        // A song appears once per playlist: adding one already there moves it.
        let mut at = at.unwrap_or(order.len()).min(order.len());
        for s in &adding {
            if let Some(i) = order.iter().position(|x| x == s) {
                order.remove(i);
                if i < at {
                    at -= 1;
                }
            }
        }
        order.splice(at..at, adding);
        self.set_order(id, order)
    }

    pub fn remove(&mut self, id: &str, indices: &[usize]) -> Result<()> {
        if id == LIBRARY_ID {
            return Err(Error::Invalid("songs leave the library by being deleted".into()));
        }
        let drop: HashSet<usize> = indices.iter().copied().collect();
        let order = self.order(id)?.into_iter().enumerate().filter(|(i, _)| !drop.contains(i)).map(|(_, s)| s).collect();
        self.set_order(id, order)
    }

    pub fn move_block(&mut self, id: &str, from: usize, count: usize, to: usize) -> Result<()> {
        let mut order = self.order(id)?;
        if order::move_block(&mut order, from, count, to) {
            self.set_order(id, order)?;
        }
        Ok(())
    }

    /// Puts `song` right after the song at `anchor`, adding it if it isn't in the playlist.
    pub fn place_after(&mut self, id: &str, anchor: usize, song: &str) -> Result<()> {
        let mut order = self.order(id)?;
        let anchor_id = order.get(anchor).cloned().ok_or_else(|| not_found("song at that position"))?;
        if self.song(song).is_none() {
            return Err(not_found("song"));
        }
        if !order.iter().any(|s| s == song) {
            order.insert(anchor + 1, song.to_owned());
        } else if !order::place_after(&mut order, &anchor_id, song) {
            return Ok(());
        }
        self.set_order(id, order)
    }

    pub fn smart_reorder(&mut self, id: &str) -> Result<()> {
        let order = self.order(id)?;
        let analyses: Vec<Option<&Analysis>> = order.iter().map(|s| self.song(s).and_then(|s| s.analysis.as_ref())).collect();
        let path = order::smart_order(order.len(), |a, b| 1.0 - compat(analyses[a], analyses[b]).score);
        let new = path.into_iter().map(|i| order[i].clone()).collect();
        self.set_order(id, new)
    }

    pub fn undo(&mut self, id: &str) -> Result<()> {
        self.step_history(id, true)
    }

    pub fn redo(&mut self, id: &str) -> Result<()> {
        self.step_history(id, false)
    }

    fn step_history(&mut self, id: &str, back: bool) -> Result<()> {
        let current = self.order(id)?;
        let history = self.history.entry(id.to_owned()).or_default();
        let (from, to) = if back { (&mut history.undo, &mut history.redo) } else { (&mut history.redo, &mut history.undo) };
        let Some(order) = from.pop() else {
            return Err(Error::Invalid(if back { "nothing to undo" } else { "nothing to redo" }.into()));
        };
        to.push(current);
        self.write_order(id, order)
    }

    // Transitions.

    fn side<'a>(&self, song: &'a Song) -> Side<'a> {
        Side { duration_ms: song.duration_ms as f64, analysis: song.analysis.as_ref() }
    }

    pub fn compat(&self, from: &str, to: &str) -> Compat {
        let analysis = |id| self.song(id).and_then(|s| s.analysis.as_ref());
        compat(analysis(from), analysis(to))
    }

    /// The transition for a pair, and whether it was edited by hand.
    pub fn transition(&self, from: &str, to: &str) -> Result<(Transition, bool)> {
        match self.store.transitions.get(&pair_key(from, to)) {
            Some(t) => Ok((self.fit(from, to, t.clone())?, true)),
            None => Ok((self.automatic(from, to)?, false)),
        }
    }

    /// The transition vimj picks for a pair by itself.
    fn automatic(&self, from: &str, to: &str) -> Result<Transition> {
        let (a, b) = (self.song(from).ok_or_else(|| not_found("song"))?, self.song(to).ok_or_else(|| not_found("song"))?);
        Ok(auto_transition(self.side(a), self.side(b), &self.compat(from, to)))
    }

    /// `t` with its cue points and length kept within both songs.
    pub fn fit(&self, from: &str, to: &str, mut t: Transition) -> Result<Transition> {
        let (a, b) = (self.song(from).ok_or_else(|| not_found("song"))?, self.song(to).ok_or_else(|| not_found("song"))?);
        t.clamp(self.side(a), self.side(b));
        Ok(t)
    }

    pub fn link(&self, from: &str, to: &str) -> Link {
        let transition = self.transition(from, to).map(|(t, _)| t).expect("both songs exist");
        Link { transition, compat: self.compat(from, to) }
    }

    pub fn transition_detail(&self, from: &str, to: &str) -> Result<TransitionDetail> {
        let (transition, saved) = self.transition(from, to)?;
        let grid = |id: &str| self.song(id).and_then(|s| s.analysis.as_ref()).and_then(|a| a.grid);
        Ok(TransitionDetail {
            from: self.info(from)?,
            to: self.info(to)?,
            compat: self.compat(from, to),
            timing: transition.timing(grid(from).as_ref(), grid(to).as_ref()),
            transition,
            saved,
            automatic: self.automatic(from, to)?,
        })
    }

    pub fn set_transition(&mut self, from: &str, to: &str, t: Transition) -> Result<TransitionDetail> {
        let mut t = self.fit(from, to, t)?;
        if from == to {
            return Err(Error::Invalid("a song can't transition into itself".into()));
        }
        t.auto = false;
        self.store.transitions.insert(pair_key(from, to), t);
        self.save();
        self.transition_detail(from, to)
    }

    pub fn reset_transition(&mut self, from: &str, to: &str) -> Result<TransitionDetail> {
        self.store.transitions.remove(&pair_key(from, to));
        self.save();
        self.transition_detail(from, to)
    }

    /// Songs from the whole library that flow best out of the one at `index`.
    pub fn suggestions(&self, id: &str, index: usize) -> Result<Vec<Suggestion>> {
        let order = self.order(id)?;
        let anchor = order.get(index).ok_or_else(|| not_found("song at that position"))?;
        let mut scored: Vec<(Compat, &Song)> = self
            .store
            .songs
            .iter()
            .filter(|s| s.id != *anchor && self.state(&s.id) != SongState::Missing)
            .map(|s| (self.compat(anchor, &s.id), s))
            .collect();
        scored.sort_by(|a, b| b.0.score.total_cmp(&a.0.score));
        Ok(scored
            .into_iter()
            .take(SUGGESTIONS)
            .map(|(compat, s)| Suggestion {
                song: SongInfo { song: s.clone(), state: self.state(&s.id) },
                compat,
                index: order.iter().position(|o| *o == s.id),
            })
            .collect())
    }

    /// What the engine needs to play `ids` in order, each song mixing into the next.
    pub fn entries(&self, ids: &[String], music_dir: &Path) -> Vec<Entry> {
        let songs: Vec<&Song> = ids.iter().filter_map(|i| self.song(i)).collect();
        songs
            .iter()
            .enumerate()
            .map(|(i, s)| Entry {
                id: s.id.clone(),
                path: music_dir.join(&s.file),
                grid: s.analysis.as_ref().and_then(|a| a.grid),
                gain_db: normalising_gain(s.analysis.as_ref().map(|a| a.loudness)),
                next: songs.get(i + 1).and_then(|next| self.transition(&s.id, &next.id).ok()).map(|(t, _)| t),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vimj_core::{Grid, Key};

    fn song(id: &str, bpm: f64, tonic: u8, energy: f32) -> Song {
        Song {
            id: id.into(),
            title: id.to_uppercase(),
            artist: "artist".into(),
            duration_ms: 200_000,
            file: format!("{id}.opus"),
            source: None,
            added: 0,
            analysis: Some(Analysis {
                version: 1,
                grid: Some(Grid { bpm, downbeat_ms: 0.0 }),
                key: Some(Key { tonic, minor: true }),
                energy,
                loudness: -9.0,
                start_ms: 0.0,
                end_ms: 199_000.0,
            }),
        }
    }

    fn library() -> Library {
        let mut lib = Library::in_memory();
        for s in [song("a", 124.0, 9, 0.6), song("b", 140.0, 2, 0.9), song("c", 125.0, 4, 0.6), song("d", 90.0, 7, 0.2)] {
            lib.add_song(s);
        }
        lib
    }

    fn ids(lib: &Library, playlist: &str) -> Vec<String> {
        lib.order(playlist).unwrap()
    }

    /// A `library.json` path in a fresh directory of its own.
    fn library_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("vimj-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("library.json")
    }

    /// Loading a library this build can't read fails, rather than starting an
    /// empty one that the first save would write over it.
    #[test]
    fn a_library_this_build_cant_read_doesnt_load() {
        let style = |volume: &str| {
            format!(
                r#"{{"transitions": {{"a>b": {{"volume": "{volume}", "eq": "off", "filter": "none", "effect": "none", "looping": "none", "bars": 4, "out_ms": 0, "in_ms": 0, "sync": true, "auto": false}}}}}}"#
            )
        };
        let path = library_file("styles");
        std::fs::write(&path, style("smooth")).unwrap();
        assert!(Library::load(path.clone()).is_ok());
        // What an older build makes of a style added since.
        std::fs::write(&path, style("spin_back")).unwrap();
        assert!(Library::load(path.clone()).is_err());

        let path = library_file("newer");
        std::fs::write(&path, format!(r#"{{"version": {}}}"#, STORE_VERSION + 1)).unwrap();
        assert!(Library::load(path).is_err(), "a newer format, even one that parses");
    }

    #[test]
    fn libraries_without_a_format_number_load_get_one_and_are_kept_for_older_builds() {
        let path = library_file("old");
        let kept = path.with_file_name("library.v0.json");
        assert!(Library::load(path.clone()).unwrap().songs().is_empty(), "no file yet");
        assert!(!kept.exists(), "nothing to keep");
        let old = r#"{"songs": [], "next_playlist": 3}"#;
        std::fs::write(&path, old).unwrap();
        Library::load(path.clone()).unwrap().save();
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["version"], STORE_VERSION);
        // Older builds can't read that and would start over empty: they get the file as it was.
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), old);
        // An older build wrote the library again since; the first copy stays.
        std::fs::write(&path, r#"{"songs": []}"#).unwrap();
        Library::load(path).unwrap();
        assert_eq!(std::fs::read_to_string(&kept).unwrap(), old);
    }

    #[test]
    fn playlists_hold_each_song_once_and_undo() {
        let mut lib = library();
        let p = lib.create_playlist("Set", vec!["a".into(), "b".into(), "a".into(), "zz".into()]).unwrap();
        assert_eq!(ids(&lib, &p.id), ["a", "b"]);
        lib.add_songs(&p.id, &["c".into(), "a".into()], Some(1)).unwrap();
        assert_eq!(ids(&lib, &p.id), ["c", "a", "b"]);
        lib.move_block(&p.id, 0, 2, 1).unwrap();
        assert_eq!(ids(&lib, &p.id), ["b", "c", "a"]);
        lib.undo(&p.id).unwrap();
        assert_eq!(ids(&lib, &p.id), ["c", "a", "b"]);
        lib.redo(&p.id).unwrap();
        assert_eq!(ids(&lib, &p.id), ["b", "c", "a"]);
        lib.remove(&p.id, &[0]).unwrap();
        assert_eq!(ids(&lib, &p.id), ["c", "a"]);
        assert!(lib.view(&p.id).unwrap().can_undo);
    }

    #[test]
    fn the_library_reorders_like_a_playlist() {
        let mut lib = library();
        lib.smart_reorder(LIBRARY_ID).unwrap();
        let order = ids(&lib, LIBRARY_ID);
        // a and c match (124/125, adjacent keys); they should end up side by side.
        let (ia, ic) = (order.iter().position(|s| s == "a").unwrap(), order.iter().position(|s| s == "c").unwrap());
        assert_eq!(ia.abs_diff(ic), 1, "{order:?}");
        lib.undo(LIBRARY_ID).unwrap();
        assert_eq!(ids(&lib, LIBRARY_ID), ["a", "b", "c", "d"]);
        assert!(lib.remove(LIBRARY_ID, &[0]).is_err());
        lib.add_songs(LIBRARY_ID, &["d".into()], Some(1)).unwrap();
        assert_eq!(ids(&lib, LIBRARY_ID), ["a", "d", "b", "c"]);
    }

    #[test]
    fn suggests_from_the_whole_library() {
        let mut lib = library();
        let p = lib.create_playlist("Set", vec!["a".into(), "b".into()]).unwrap();
        let s = lib.suggestions(&p.id, 0).unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].song.song.id, "c");
        assert_eq!(s[0].index, None, "c isn't in the playlist yet");
        assert!(s.windows(2).all(|w| w[0].compat.score >= w[1].compat.score));
        lib.place_after(&p.id, 0, "c").unwrap();
        assert_eq!(ids(&lib, &p.id), ["a", "c", "b"]);
        lib.place_after(&p.id, 0, "b").unwrap();
        assert_eq!(ids(&lib, &p.id), ["a", "b", "c"]);
    }

    #[test]
    fn edited_transitions_stick_and_reset_to_auto() {
        let mut lib = library();
        let (auto, saved) = lib.transition("a", "c").unwrap();
        assert!(auto.auto && !saved);
        let mut edited = auto.clone();
        edited.bars = 2;
        edited.in_ms = 1e9;
        let detail = lib.set_transition("a", "c", edited).unwrap();
        assert!(detail.saved && !detail.transition.auto && detail.transition.bars == 2);
        assert!(detail.transition.in_ms < 200_000.0, "clamped into the song");
        assert!(lib.reset_transition("a", "c").unwrap().transition.auto);
    }

    #[test]
    fn deleting_a_song_cleans_up_after_it() {
        let mut lib = library();
        let p = lib.create_playlist("Set", vec!["a".into(), "b".into()]).unwrap();
        lib.set_transition("a", "b", lib.transition("a", "b").unwrap().0).unwrap();
        lib.remove_song("b").unwrap();
        assert_eq!(ids(&lib, &p.id), ["a"]);
        assert!(lib.store.transitions.is_empty());
    }

    #[test]
    fn entries_carry_a_transition_into_each_next_song() {
        let lib = library();
        let order = ids(&lib, LIBRARY_ID);
        let entries = lib.entries(&order, Path::new("/music"));
        assert!(entries[..3].iter().all(|e| e.next.is_some()) && entries[3].next.is_none());
        assert_eq!(entries[0].path, Path::new("/music/a.opus"));
        assert!((entries[0].gain_db - -1.0).abs() < 1e-6);
    }
}
