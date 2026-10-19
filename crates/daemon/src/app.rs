//! Shared daemon state.

use crate::config::{Config, Paths};
use crate::downloads::Downloads;
use crate::library::Library;
use crate::playback::Playback;
use anyhow::Result;
use vimj_audio::player::Output;
use vimj_core::protocol::{ConfigUpdate, Event, Level, PROTOCOL, PlayerState, Status};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, broadcast};

/// Lock order: `lib` before anything inside `playback`.
pub struct App {
    pub paths: Paths,
    pub config: Mutex<Config>,
    pub music_dir: PathBuf,
    pub lib: Mutex<Library>,
    pub playback: Playback,
    pub downloads: Downloads,
    pub events: broadcast::Sender<Event>,
    /// Wakes the analysis worker.
    pub analyzer: Notify,
    pub shutdown: Notify,
    tools: Mutex<Tools>,
}

#[derive(Clone, Default)]
struct Tools {
    yt_dlp: Option<String>,
    ffmpeg: bool,
}

impl App {
    pub fn new(paths: Paths, output: Output) -> Result<Arc<App>> {
        let config = Config::load(&paths.config)?;
        let music_dir = config.music_dir(&paths);
        let lib = Library::load(paths.library.clone())?;
        let (events, _) = broadcast::channel(256);
        let (changed, mut changes) = tokio::sync::mpsc::unbounded_channel();
        let playback = Playback::new(output, config.ffmpeg(), config.volume.unwrap_or(80), changed);
        let app = Arc::new(App {
            paths,
            config: Mutex::new(config),
            music_dir,
            lib: Mutex::new(lib),
            playback,
            downloads: Downloads::default(),
            events,
            analyzer: Notify::new(),
            shutdown: Notify::new(),
            tools: Mutex::new(Tools::default()),
        });
        // Relay the engine's progress as player events.
        let relay = Arc::downgrade(&app);
        tokio::spawn(async move {
            while changes.recv().await.is_some() {
                while changes.try_recv().is_ok() {}
                let Some(app) = relay.upgrade() else { return };
                let (state, error) = app.player_state_and_error();
                app.publish(Event::Player(state));
                if let Some((title, message)) = error {
                    app.toast(Level::Error, format!("Can't play {title}: {message}"));
                }
            }
        });
        Ok(app)
    }

    pub fn publish(&self, event: Event) {
        let _ = self.events.send(event);
    }

    pub fn toast(&self, level: Level, message: impl Into<String>) {
        self.publish(Event::Toast { level, message: message.into() });
    }

    pub fn publish_downloads(&self) {
        self.publish(Event::Downloads { downloads: self.downloads.list() });
    }

    /// Songs, playlists or transitions changed: keep the engine's set current.
    pub fn library_changed(&self) {
        let lib = self.lib.lock().expect("library lock");
        self.playback.refresh(&lib, &self.music_dir);
    }

    pub fn player_state(&self) -> PlayerState {
        self.player_state_and_error().0
    }

    fn player_state_and_error(&self) -> (PlayerState, Option<(String, String)>) {
        let lib = self.lib.lock().expect("library lock");
        self.playback.state(&lib)
    }

    pub fn status(&self) -> Status {
        let tools = self.tools.lock().expect("tools lock").clone();
        let cookies_from_browser = self.config.lock().expect("config lock").cookies_from_browser.clone();
        Status {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").to_owned(),
            music_dir: self.music_dir.display().to_string(),
            yt_dlp: tools.yt_dlp,
            ffmpeg: tools.ffmpeg,
            audio: self.playback.device(),
            cookies_from_browser,
        }
    }

    pub fn update_config(&self, update: ConfigUpdate) -> Result<Status> {
        {
            let mut config = self.config.lock().expect("config lock");
            if let Some(browser) = update.cookies_from_browser {
                let browser = browser.trim().to_lowercase();
                config.cookies_from_browser = (!browser.is_empty()).then_some(browser);
            }
            config.save(&self.paths.config)?;
        }
        let status = self.status();
        self.publish(Event::Status(status.clone()));
        Ok(status)
    }

    /// Checks that yt-dlp and ffmpeg run.
    pub async fn probe_tools(&self) {
        let config = self.config.lock().expect("config lock").clone();
        let version = |program: PathBuf, arg: &'static str| async move {
            let out = tokio::process::Command::new(program).arg(arg).output().await.ok()?;
            out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_owned())
        };
        let yt_dlp = version(config.yt_dlp(), "--version").await;
        let ffmpeg = version(config.ffmpeg(), "-version").await.is_some();
        if yt_dlp.is_none() {
            tracing::warn!("{} doesn't run; downloads won't work", config.yt_dlp().display());
        }
        if !ffmpeg {
            tracing::warn!("{} doesn't run; songs can't be decoded", config.ffmpeg().display());
        }
        *self.tools.lock().expect("tools lock") = Tools { yt_dlp, ffmpeg };
        self.publish(Event::Status(self.status()));
    }

    pub fn save_volume(&self, volume: u8) {
        let mut config = self.config.lock().expect("config lock");
        config.volume = Some(volume);
        if let Err(e) = config.save(&self.paths.config) {
            tracing::warn!("saving the volume: {e:#}");
        }
    }
}
