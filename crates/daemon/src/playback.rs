//! Connects the library to the audio engine: builds the set to play from a
//! playlist, keeps the engine's copy current as the playlist changes, and
//! reports what is playing.

use crate::library::{self, Library};
use vimj_audio::engine::{Command, Plan, State};
use vimj_audio::player::{Output, Player};
use vimj_core::protocol::{Mixing, PlayerState, PlayerStatus};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tokio::sync::mpsc::UnboundedSender;

/// Context reported while the editor previews a transition.
pub const PREVIEW: &str = "preview";

pub struct Playback {
    player: Player,
    current: Mutex<Current>,
}

#[derive(Default)]
struct Current {
    /// Playlist being played.
    context: Option<String>,
    /// The engine's set, as song ids.
    ids: Vec<String>,
    /// Pair being previewed.
    preview: Option<(String, String)>,
    /// The last load error already reported.
    reported: Option<(String, String)>,
}

impl Playback {
    /// `changed` gets a message whenever the engine's state moves on.
    pub fn new(output: Output, ffmpeg: PathBuf, volume: u8, changed: UnboundedSender<()>) -> Playback {
        let player = Player::start(output, ffmpeg, move |_| {
            let _ = changed.send(());
        });
        player.send(Command::Volume(f32::from(volume.min(100)) / 100.0));
        Playback { player, current: Mutex::new(Current::default()) }
    }

    pub fn device(&self) -> Result<String, String> {
        self.player.device.clone()
    }

    pub fn play(&self, lib: &Library, music_dir: &Path, context: &str, index: usize, position_ms: f64) -> library::Result<()> {
        let ids = lib.order(context)?;
        if index >= ids.len() {
            return Err(library::Error::NotFound("song at that position".into()));
        }
        let entries = lib.entries(&ids, lib.mix(context)?, music_dir);
        self.player.send(Command::SetPlan(Plan { entries, preview: None }));
        self.player.send(Command::Play { index, position_ms });
        let mut current = self.current.lock().expect("playback lock");
        current.context = Some(context.to_owned());
        current.ids = ids;
        current.preview = None;
        Ok(())
    }

    /// Loops the transition between two songs, starting `lead_in_ms` before it.
    pub fn preview(&self, lib: &Library, music_dir: &Path, from: &str, to: &str, lead_in_ms: f64) -> library::Result<()> {
        let (transition, _) = lib.transition(from, to)?;
        let ids = vec![from.to_owned(), to.to_owned()];
        let entries = lib.entries(&ids, true, music_dir);
        self.player.send(Command::SetPlan(Plan { entries, preview: Some(lead_in_ms) }));
        self.player.send(Command::Play { index: 0, position_ms: transition.out_ms - lead_in_ms });
        let mut current = self.current.lock().expect("playback lock");
        current.context = Some(PREVIEW.to_owned());
        current.ids = ids;
        current.preview = Some((from.to_owned(), to.to_owned()));
        Ok(())
    }

    /// Something in the library changed: give the engine the new set.
    pub fn refresh(&self, lib: &Library, music_dir: &Path) {
        let mut current = self.current.lock().expect("playback lock");
        let plan = if let Some((from, to)) = &current.preview {
            if lib.song(from).is_none() || lib.song(to).is_none() {
                return;
            }
            let ids = [from.clone(), to.clone()];
            let lead = 4_000.0;
            Plan { entries: lib.entries(&ids, true, music_dir), preview: Some(lead) }
        } else if let Some(context) = &current.context {
            let (Ok(ids), Ok(mix)) = (lib.order(context), lib.mix(context)) else { return };
            let entries = lib.entries(&ids, mix, music_dir);
            current.ids = ids;
            Plan { entries, preview: None }
        } else {
            return;
        };
        self.player.send(Command::SetPlan(plan));
    }

    pub fn toggle(&self) {
        match self.player.snapshot().state {
            State::Playing => self.player.send(Command::Pause),
            State::Paused => self.player.send(Command::Resume),
            State::Stopped | State::Loading => {}
        }
    }

    pub fn send(&self, command: Command) {
        if matches!(command, Command::Stop) {
            let mut current = self.current.lock().expect("playback lock");
            current.preview = None;
        }
        self.player.send(command);
    }

    /// What is playing, plus a load error not reported yet.
    pub fn state(&self, lib: &Library) -> (PlayerState, Option<(String, String)>) {
        let snapshot = self.player.snapshot();
        let mut current = self.current.lock().expect("playback lock");
        let song_at = |i: usize| current.ids.get(i).and_then(|id| lib.song(id)).cloned();
        let song = snapshot.index.and_then(song_at);
        let next = snapshot.index.and_then(|i| song_at(i + 1));
        let mixing_ids = current.context.as_deref().is_some_and(|c| c == PREVIEW || lib.mix(c).unwrap_or(false));
        let transition = match (&song, &next) {
            (Some(a), Some(b)) if mixing_ids => lib.transition(&a.id, &b.id).ok().map(|(t, _)| t),
            _ => None,
        };
        let mixing = snapshot.incoming.and_then(|(i, position_ms, progress)| {
            song_at(i).map(|song| Mixing { song, position_ms, progress })
        });
        let state = PlayerState {
            status: match snapshot.state {
                State::Stopped => PlayerStatus::Stopped,
                State::Loading => PlayerStatus::Loading,
                State::Playing => PlayerStatus::Playing,
                State::Paused => PlayerStatus::Paused,
            },
            context: current.context.clone(),
            index: snapshot.index,
            song,
            position_ms: snapshot.position_ms,
            rate: snapshot.tempo,
            mixing,
            next,
            transition,
            volume: (snapshot.volume * 100.0).round() as u8,
            preview: current.preview.is_some(),
        };
        let error = snapshot.error.filter(|e| current.reported.as_ref() != Some(e));
        if let Some(e) = &error {
            current.reported = Some(e.clone());
        }
        let error = error.map(|(id, message)| (lib.song(&id).map_or(id, |s| s.title.clone()), message));
        (state, error)
    }
}
