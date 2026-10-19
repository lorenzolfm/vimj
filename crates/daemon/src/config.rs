use anyhow::{Context, Result};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::path::{Path, PathBuf};

pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub library: PathBuf,
    pub waveforms: PathBuf,
    /// Downloads land here before moving into the music directory.
    pub incoming: PathBuf,
}

impl Paths {
    pub fn new() -> Result<Paths> {
        let config_dir = match std::env::var_os("VIMJ_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::config_dir().context("no config directory")?.join("vimj"),
        };
        let data = match std::env::var_os("VIMJ_DATA_DIR") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::data_dir().context("no data directory")?.join("vimj"),
        };
        let paths = Paths {
            config: config_dir.join("config.toml"),
            library: data.join("library.json"),
            waveforms: data.join("waveforms"),
            incoming: data.join("incoming"),
            data,
        };
        for dir in [&config_dir, &paths.data, &paths.waveforms, &paths.incoming] {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        Ok(paths)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Config {
    /// Where songs are kept. Defaults to `~/Music/vimj`.
    pub music_dir: Option<PathBuf>,
    pub yt_dlp: Option<PathBuf>,
    pub ffmpeg: Option<PathBuf>,
    pub ffprobe: Option<PathBuf>,
    /// Passed to `yt-dlp --cookies-from-browser`, e.g. "firefox", for when
    /// YouTube wants a signed-in user. A Premium login may also unlock
    /// higher-bitrate streams where YouTube offers them.
    pub cookies_from_browser: Option<String>,
    /// 0..=100.
    pub volume: Option<u8>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let mut config: Config = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        if let Some(dir) = std::env::var_os("VIMJ_MUSIC_DIR") {
            config.music_dir = Some(PathBuf::from(dir));
        }
        if let Some(bin) = std::env::var_os("VIMJ_YT_DLP") {
            config.yt_dlp = Some(PathBuf::from(bin));
        }
        Ok(config)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        write_atomic(path, toml::to_string_pretty(self)?.as_bytes())
    }

    /// The music directory, created if needed; falls back to the data directory.
    pub fn music_dir(&self, paths: &Paths) -> PathBuf {
        let dir = self.music_dir.clone().unwrap_or_else(|| {
            dirs::audio_dir().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join("Music")).join("vimj")
        });
        match std::fs::create_dir_all(&dir) {
            Ok(()) => dir,
            Err(e) => {
                let fallback = paths.data.join("music");
                tracing::warn!("can't use {}: {e}; keeping songs in {}", dir.display(), fallback.display());
                let _ = std::fs::create_dir_all(&fallback);
                fallback
            }
        }
    }

    pub fn yt_dlp(&self) -> PathBuf {
        self.yt_dlp.clone().unwrap_or_else(|| "yt-dlp".into())
    }

    pub fn ffmpeg(&self) -> PathBuf {
        self.ffmpeg.clone().unwrap_or_else(|| "ffmpeg".into())
    }

    pub fn ffprobe(&self) -> PathBuf {
        self.ffprobe.clone().unwrap_or_else(|| "ffprobe".into())
    }
}

/// `None` when there is no file yet. A file that is there but can't be read
/// is an error rather than an empty value, which would be saved over it.
pub fn load_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_slice(&bytes).map(Some).with_context(|| format!("reading {}", path.display()))
}

pub fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    write_atomic(path, &serde_json::to_vec(value)?)
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
