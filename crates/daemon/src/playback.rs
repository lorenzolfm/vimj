//! Connects the library to the audio engine: builds the set to play from a
//! playlist, keeps the engine's copy current as the playlist changes, and
//! reports what is playing.

use crate::library::{self, Library};
use vimj_audio::engine::{Command, Plan, State};
use vimj_audio::player::{Output, Player};
use vimj_core::Transition;
use vimj_core::protocol::{Link, Mixing, PlayerState, PlayerStatus};
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
    preview: Option<Preview>,
    /// The last load error already reported.
    reported: Option<(String, String)>,
}

/// A transition looping in the editor.
struct Preview {
    from: String,
    to: String,
    lead_in_ms: f64,
    /// The editor's unsaved version, played instead of the stored one.
    draft: Option<Transition>,
}

impl Preview {
    fn plan(&self, lib: &Library, music_dir: &Path) -> Plan {
        let mut entries = lib.entries(&[self.from.clone(), self.to.clone()], music_dir);
        if let (Some(first), Some(t)) = (entries.first_mut(), &self.draft) {
            first.next = Some(t.clone());
        }
        Plan { entries, preview: Some(self.lead_in_ms) }
    }
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
        let entries = lib.entries(&ids, music_dir);
        self.player.send(Command::SetPlan(Plan { entries, preview: None }));
        self.player.send(Command::Play { index, position_ms });
        let mut current = self.current.lock().expect("playback lock");
        current.context = Some(context.to_owned());
        current.ids = ids;
        current.preview = None;
        Ok(())
    }

    /// Loops the transition between two songs, starting `lead_in_ms` before
    /// it: `draft` if given, or else the stored one.
    pub fn preview(&self, lib: &Library, music_dir: &Path, from: &str, to: &str, lead_in_ms: f64, draft: Option<Transition>) -> library::Result<()> {
        let draft = draft.map(|t| lib.fit(from, to, t)).transpose()?;
        let out_ms = match &draft {
            Some(t) => t.out_ms,
            None => lib.transition(from, to)?.0.out_ms,
        };
        let preview = Preview { from: from.to_owned(), to: to.to_owned(), lead_in_ms, draft };
        self.player.send(Command::SetPlan(preview.plan(lib, music_dir)));
        self.player.send(Command::Play { index: 0, position_ms: out_ms - lead_in_ms });
        let mut current = self.current.lock().expect("playback lock");
        current.context = Some(PREVIEW.to_owned());
        current.ids = vec![preview.from.clone(), preview.to.clone()];
        current.preview = Some(preview);
        Ok(())
    }

    /// Plays `draft` in the running preview from now on, without starting over.
    pub fn update_preview(&self, lib: &Library, music_dir: &Path, draft: Transition) -> library::Result<()> {
        let mut current = self.current.lock().expect("playback lock");
        let preview = current.preview.as_mut().ok_or_else(|| library::Error::NotFound("preview".into()))?;
        preview.draft = Some(lib.fit(&preview.from, &preview.to, draft)?);
        self.player.send(Command::SetPlan(preview.plan(lib, music_dir)));
        Ok(())
    }

    /// Something in the library changed: give the engine the new set.
    pub fn refresh(&self, lib: &Library, music_dir: &Path) {
        let mut current = self.current.lock().expect("playback lock");
        let plan = if let Some(preview) = &current.preview {
            if lib.song(&preview.from).is_none() || lib.song(&preview.to).is_none() {
                return;
            }
            preview.plan(lib, music_dir)
        } else if let Some(context) = &current.context {
            let Ok(ids) = lib.order(context) else { return };
            let entries = lib.entries(&ids, music_dir);
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
        let draft = current.preview.as_ref().and_then(|p| p.draft.clone());
        let transition = match (&song, &next) {
            (Some(a), Some(b)) => {
                let link = lib.link(&a.id, &b.id);
                Some(Link { transition: draft.unwrap_or(link.transition), ..link })
            }
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
