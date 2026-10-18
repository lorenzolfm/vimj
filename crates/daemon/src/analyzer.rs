//! Keeps the library in step with the music directory, and analyses songs
//! in the background, one at a time.

use crate::app::App;
use crate::downloads::now;
use anyhow::{Context, Result};
use vimj_audio::{analysis, pcm};
use vimj_core::meta::{Raw, from_file_name, song_name};
use vimj_core::protocol::{Event, Level, SongState};
use vimj_core::Song;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

/// Analysis always decodes at this rate, whatever the sound card runs at.
const ANALYSIS_RATE: u32 = 48_000;
const AUDIO_EXTENSIONS: [&str; 11] = ["opus", "ogg", "oga", "m4a", "mp3", "flac", "wav", "aac", "webm", "aiff", "wma"];

/// Adds audio files nobody told vimj about, and flags songs whose file is gone.
pub fn rescan(app: &Arc<App>) -> Result<usize> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(&app.music_dir).with_context(|| format!("reading {}", app.music_dir.display()))? {
        let path = entry?.path();
        let ext = path.extension().and_then(|e| e.to_str()).map(str::to_lowercase).unwrap_or_default();
        if path.is_file() && AUDIO_EXTENSIONS.contains(&ext.as_str())
            && let Some(name) = path.file_name().and_then(|n| n.to_str())
        {
            found.push(name.to_owned());
        }
    }
    let new: Vec<String> = {
        let mut lib = app.lib.lock().expect("library lock");
        let missing: Vec<String> =
            lib.songs().iter().filter(|s| !app.music_dir.join(&s.file).exists()).map(|s| s.id.clone()).collect();
        for id in missing {
            lib.set_state(&id, SongState::Missing);
        }
        found.into_iter().filter(|f| !lib.has_file(f)).collect()
    };
    let ffprobe = app.config.lock().expect("config lock").ffprobe();
    let mut added = 0;
    for file in new {
        let path = app.music_dir.join(&file);
        let probe = probe(&ffprobe, &path);
        let (artist, title) = match probe.as_ref().and_then(|p| p.title.as_deref()) {
            Some(title) => song_name(Raw { title, artist: probe.as_ref().and_then(|p| p.artist.as_deref()), track: Some(title), uploader: None }),
            None => from_file_name(&file),
        };
        let song = Song {
            id: local_id(&file),
            title: if title.is_empty() { file.clone() } else { title },
            artist,
            duration_ms: probe.as_ref().map_or(0, |p| p.duration_ms),
            file,
            source: None,
            added: now(),
            analysis: None,
        };
        app.lib.lock().expect("library lock").add_song(song);
        added += 1;
    }
    if added > 0 {
        app.publish(Event::Library);
        app.publish(Event::Playlists);
        app.library_changed();
        app.analyzer.notify_one();
    }
    Ok(added)
}

/// A stable id for a file found in the music directory.
fn local_id(file: &str) -> String {
    // FNV-1a: stable across runs, unlike the standard hasher.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in file.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("local-{hash:012x}")
}

pub struct Probe {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: u64,
}

pub fn probe(ffprobe: &Path, path: &Path) -> Option<Probe> {
    #[derive(Deserialize)]
    struct Output {
        format: Format,
    }
    #[derive(Deserialize)]
    struct Format {
        duration: Option<String>,
        #[serde(default)]
        tags: HashMap<String, String>,
    }
    let output = std::process::Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration:format_tags", "-of", "json"])
        .arg(path)
        .output()
        .ok()?;
    let parsed: Output = serde_json::from_slice(&output.stdout).ok()?;
    let tag = |name: &str| {
        parsed.format.tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim().to_owned()).filter(|v| !v.is_empty())
    };
    Some(Probe {
        title: tag("title"),
        artist: tag("artist"),
        duration_ms: parsed.format.duration.and_then(|d| d.parse::<f64>().ok()).map_or(0, |d| (d * 1000.0) as u64),
    })
}

pub async fn run(app: Arc<App>) {
    loop {
        let next = app.lib.lock().expect("library lock").needs_analysis(analysis::VERSION);
        let Some(song) = next else {
            app.analyzer.notified().await;
            continue;
        };
        let path = app.music_dir.join(&song.file);
        let ffmpeg = app.config.lock().expect("config lock").ffmpeg();
        let waveform_path = app.paths.waveforms.join(format!("{}.bin", song.id));
        let result = tokio::task::spawn_blocking(move || -> Result<(analysis::Output, u64)> {
            let decoded = pcm::decode(&ffmpeg, &path, ANALYSIS_RATE)?;
            let duration = decoded.duration_ms() as u64;
            let out = analysis::analyze(&decoded);
            crate::config::write_atomic(&waveform_path, &out.waveform)?;
            Ok((out, duration))
        })
        .await;
        let ok = match result {
            Ok(Ok((out, duration))) => {
                let mut lib = app.lib.lock().expect("library lock");
                let _ = lib.set_analysis(&song.id, out.analysis, duration);
                true
            }
            Ok(Err(e)) => {
                tracing::warn!("analysing {}: {e:#}", song.file);
                app.lib.lock().expect("library lock").set_state(&song.id, SongState::Failed);
                app.toast(Level::Warn, format!("Couldn't analyse {}: {e:#}", song.title));
                false
            }
            Err(e) => {
                tracing::error!("analysis task: {e}");
                app.lib.lock().expect("library lock").set_state(&song.id, SongState::Failed);
                false
            }
        };
        if app.downloads.song_ready(&song.id, ok) {
            app.publish_downloads();
        }
        app.publish(Event::Library);
        app.library_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ids_are_stable_and_url_safe() {
        assert_eq!(local_id("Song.mp3"), local_id("Song.mp3"));
        assert_ne!(local_id("Song.mp3"), local_id("Song.flac"));
        assert!(local_id("ü ß.mp3").chars().all(|c| c.is_ascii_alphanumeric() || c == '-'));
    }
}
