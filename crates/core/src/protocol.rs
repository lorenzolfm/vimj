//! Wire types for the daemon's HTTP API (`/api/...`) and its event stream
//! (`/api/events`, server-sent events carrying one JSON [`Event`] each).

use crate::compat::Compat;
use crate::model::Song;
use crate::transition::{Timing, Transition};
use serde::{Deserialize, Serialize};

pub const DEFAULT_PORT: u16 = 8787;
/// What the daemon's [`Status`] says it speaks. Bumped whenever the API
/// changes so that frontends and daemons of different builds can't talk.
pub const PROTOCOL: u32 = 2;
/// Playlist id of the whole library, which can be ordered and mixed like any playlist.
pub const LIBRARY_ID: &str = "library";
/// How many suggestions the "transition to…" menu offers.
pub const SUGGESTIONS: usize = 5;
/// Waveform columns per second, see `GET /api/songs/{id}/waveform`.
pub const WAVEFORM_HZ: f64 = 100.0;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// [`PROTOCOL`] as the daemon was built. Frontends talk only to a daemon
    /// that speaks theirs, and take one without it for another program.
    pub protocol: u32,
    pub version: String,
    pub music_dir: String,
    /// `yt-dlp --version`, or `None` when it can't be run.
    pub yt_dlp: Option<String>,
    pub ffmpeg: bool,
    /// Output device name, or what went wrong opening it.
    pub audio: Result<String, String>,
    /// Browser whose YouTube login downloads use.
    pub cookies_from_browser: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConfigUpdate {
    /// Empty clears it.
    pub cookies_from_browser: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SongState {
    Ready,
    /// Waiting for, or in, analysis.
    Analyzing,
    /// Analysis failed; the song still plays, unranked.
    Failed,
    /// The file is gone from the music directory.
    Missing,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongInfo {
    #[serde(flatten)]
    pub song: Song,
    pub state: SongState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    pub count: usize,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playlists {
    pub library: PlaylistSummary,
    pub playlists: Vec<PlaylistSummary>,
}

/// Where a pair's transition comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Source {
    /// Worked out from both songs each time, so it follows their analysis.
    Automatic,
    /// Edited by hand and stored, at revision `rev`. Writes name the source
    /// they last saw, so that one made over someone else's edit is refused.
    Edited { rev: u64 },
}

/// How one song flows into the next.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub transition: Transition,
    pub compat: Compat,
    pub source: Source,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlaylistView {
    pub id: String,
    pub name: String,
    pub songs: Vec<SongInfo>,
    /// `links[i]` joins `songs[i]` and `songs[i + 1]`.
    pub links: Vec<Link>,
    pub can_undo: bool,
    pub can_redo: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    pub song: SongInfo,
    pub compat: Compat,
    /// Where the song already is in the playlist.
    pub index: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TransitionDetail {
    pub from: SongInfo,
    pub to: SongInfo,
    pub compat: Compat,
    pub transition: Transition,
    pub timing: Timing,
    pub source: Source,
    /// What vimj picks for the pair by itself, which resetting goes back to.
    pub automatic: Transition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    /// Looking up what the link points to.
    Resolving,
    Downloading,
    Analyzing,
    Done,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Download {
    pub id: u64,
    pub url: String,
    pub title: Option<String>,
    pub state: DownloadState,
    /// 0..=1 while downloading.
    pub progress: f32,
    /// Bytes per second while downloading.
    pub speed: Option<f64>,
    pub error: Option<String>,
    pub song_id: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerStatus {
    Stopped,
    Loading,
    Playing,
    Paused,
}

/// The song being mixed in during a transition.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mixing {
    pub song: Song,
    pub position_ms: f64,
    /// 0..=1 through the transition window.
    pub progress: f32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlayerState {
    pub status: PlayerStatus,
    /// Playlist id being played.
    pub context: Option<String>,
    /// Index of `song` in that playlist.
    pub index: Option<usize>,
    pub song: Option<Song>,
    pub position_ms: f64,
    /// Current tempo relative to the song's own, during and after synced transitions.
    pub rate: f64,
    pub mixing: Option<Mixing>,
    pub next: Option<Song>,
    /// How `song` flows into `next`. A preview plays the editor's draft,
    /// with the source of what is stored for the pair.
    pub transition: Option<Link>,
    pub volume: u8,
    /// Looping a single transition from the editor.
    pub preview: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Info,
    Warn,
    Error,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Event {
    Status(Status),
    Player(PlayerState),
    /// Songs were added, removed, renamed or analysed.
    Library,
    /// The playlist list changed.
    Playlists,
    /// A playlist's songs, order or transitions changed.
    Playlist { id: String },
    Downloads { downloads: Vec<Download> },
    Toast { level: Level, message: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DownloadRequest {
    pub url: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SongUpdate {
    pub title: Option<String>,
    pub artist: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CreatePlaylist {
    pub name: String,
    #[serde(default)]
    pub songs: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PlaylistUpdate {
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AddSongs {
    pub songs: Vec<String>,
    /// Insert before this index; appends when missing.
    pub at: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RemoveSongs {
    pub indices: Vec<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MoveSongs {
    pub from: usize,
    pub count: usize,
    /// Index the block starts at afterwards.
    pub to: usize,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlaceAfter {
    pub anchor: usize,
    pub song: String,
}

/// `PUT /api/transitions/{from}/{to}`: stores `transition` as the pair's
/// edit, if the pair is still as `base` says the client last saw it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SaveTransition {
    pub base: Source,
    pub transition: Transition,
}

/// `DELETE /api/transitions/{from}/{to}`: forgets the pair's edit, back to
/// its automatic transition, on the same terms.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ForgetTransition {
    pub base: Source,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlayRequest {
    pub context: String,
    pub index: usize,
    #[serde(default)]
    pub position_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PreviewRequest {
    pub from: String,
    pub to: String,
    /// How much of the outgoing song plays before the window.
    #[serde(default = "default_lead")]
    pub lead_in_ms: f64,
    /// Play this rather than the stored transition, for edits not saved yet.
    #[serde(default)]
    pub transition: Option<Transition>,
}

fn default_lead() -> f64 {
    4_000.0
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SeekRequest {
    pub position_ms: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VolumeRequest {
    pub volume: u8,
}
