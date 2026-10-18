//! Downloading songs with yt-dlp, one at a time. The best audio stream is
//! kept as it is (no re-encoding), so quality is whatever the site serves:
//! on YouTube that is usually Opus at about 130 kbps.

use crate::app::App;
use anyhow::{Context, Result, anyhow, bail};
use vimj_core::meta::{Raw, song_name};
use vimj_core::protocol::{Download, DownloadState, Event, Level, SongState};
use vimj_core::Song;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::Notify;

/// Prefix of the progress lines yt-dlp is told to print.
const PROGRESS: &str = "vimj-progress";
/// Pause between yt-dlp's requests to a site, and between songs. YouTube
/// flags connections that fire requests quickly, which playlist imports would.
const SLEEP_REQUESTS: &str = "0.5";
const BETWEEN_SONGS: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Default)]
pub struct Downloads {
    jobs: Mutex<Vec<Job>>,
    next: AtomicU64,
    pub wake: Notify,
}

struct Job {
    info: Download,
    cancel: Arc<Notify>,
}

impl Downloads {
    pub fn list(&self) -> Vec<Download> {
        self.jobs.lock().expect("downloads lock").iter().map(|j| j.info.clone()).collect()
    }

    pub fn add(&self, url: &str, title: Option<String>) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let info = Download {
            id,
            url: url.trim().to_owned(),
            title,
            state: DownloadState::Queued,
            progress: 0.0,
            speed: None,
            error: None,
            song_id: None,
        };
        self.jobs.lock().expect("downloads lock").push(Job { info, cancel: Arc::new(Notify::new()) });
        self.wake.notify_one();
        id
    }

    /// Cancels a job that is still going, or clears a finished one from the list.
    pub fn dismiss(&self, id: u64) -> bool {
        let mut jobs = self.jobs.lock().expect("downloads lock");
        let Some(at) = jobs.iter().position(|j| j.info.id == id) else { return false };
        match jobs[at].info.state {
            DownloadState::Resolving | DownloadState::Downloading => jobs[at].cancel.notify_one(),
            _ => {
                jobs.remove(at);
            }
        }
        true
    }

    pub fn clear_finished(&self) {
        self.jobs.lock().expect("downloads lock").retain(|j| !matches!(j.info.state, DownloadState::Done | DownloadState::Failed));
    }

    fn update(&self, id: u64, f: impl FnOnce(&mut Download)) {
        if let Some(job) = self.jobs.lock().expect("downloads lock").iter_mut().find(|j| j.info.id == id) {
            f(&mut job.info);
        }
    }

    fn take_queued(&self) -> Option<(Download, Arc<Notify>)> {
        let mut jobs = self.jobs.lock().expect("downloads lock");
        let job = jobs.iter_mut().find(|j| j.info.state == DownloadState::Queued)?;
        job.info.state = DownloadState::Resolving;
        Some((job.info.clone(), job.cancel.clone()))
    }

    /// Analysis of a downloaded song finished.
    pub fn song_ready(&self, song_id: &str, ok: bool) -> bool {
        let mut changed = false;
        for job in self.jobs.lock().expect("downloads lock").iter_mut() {
            if job.info.song_id.as_deref() == Some(song_id) && job.info.state == DownloadState::Analyzing {
                job.info.state = if ok { DownloadState::Done } else { DownloadState::Failed };
                if !ok {
                    job.info.error = Some("downloaded, but the analysis failed; it plays unranked".into());
                }
                changed = true;
            }
        }
        changed
    }
}

pub async fn run(app: Arc<App>) {
    loop {
        let Some((job, cancel)) = app.downloads.take_queued() else {
            app.downloads.wake.notified().await;
            continue;
        };
        app.publish_downloads();
        let id = job.id;
        let result = tokio::select! {
            r = process(&app, &job) => r,
            _ = cancel.notified() => Err(anyhow!("cancelled")),
        };
        let _ = tokio::fs::remove_dir_all(app.paths.incoming.join(id.to_string())).await;
        match result {
            Ok(Outcome::Added(song)) => {
                app.downloads.update(id, |d| {
                    d.state = DownloadState::Analyzing;
                    d.progress = 1.0;
                    d.speed = None;
                    d.title = Some(song.title.clone());
                    d.song_id = Some(song.id.clone());
                });
                app.toast(Level::Info, format!("Downloaded {} — {}", display_artist(&song), song.title));
                app.analyzer.notify_one();
            }
            Ok(Outcome::Known(song)) => {
                app.downloads.update(id, |d| {
                    d.state = DownloadState::Done;
                    d.title = Some(song.title.clone());
                    d.song_id = Some(song.id.clone());
                    d.error = Some("already in your library".into());
                });
            }
            Ok(Outcome::Expanded { title, count }) => {
                app.downloads.update(id, |d| {
                    d.state = DownloadState::Done;
                    d.title = Some(format!("{title} · {count} songs queued"));
                });
            }
            Err(e) => {
                let message = format!("{e:#}");
                tracing::warn!("download {} failed: {message}", job.url);
                app.downloads.update(id, |d| {
                    d.state = DownloadState::Failed;
                    d.speed = None;
                    d.error = Some(message.clone());
                });
                if message != "cancelled" {
                    app.toast(Level::Error, format!("Download failed: {message}"));
                }
            }
        }
        app.publish_downloads();
        tokio::time::sleep(BETWEEN_SONGS).await;
    }
}

fn display_artist(song: &Song) -> &str {
    if song.artist.is_empty() { "Unknown artist" } else { &song.artist }
}

enum Outcome {
    Added(Song),
    Known(Song),
    /// A playlist link, turned into one job per song.
    Expanded { title: String, count: usize },
}

async fn process(app: &Arc<App>, job: &Download) -> Result<Outcome> {
    if is_playlist(&job.url) {
        return expand_playlist(app, &job.url).await;
    }
    if let Some(video) = youtube_id(&job.url)
        && let Some(song) = app.lib.lock().expect("library lock").song(&video).cloned()
    {
        return Ok(Outcome::Known(song));
    }
    let dir = app.paths.incoming.join(job.id.to_string());
    tokio::fs::create_dir_all(&dir).await?;
    let config = app.config.lock().expect("config lock").clone();
    let mut command = Command::new(config.yt_dlp());
    command
        .args(["--no-playlist", "--newline", "--no-colors", "--ignore-config"])
        .args(["-f", "bestaudio/best", "--extract-audio", "--embed-metadata", "--write-info-json"])
        .args(["--sleep-requests", SLEEP_REQUESTS])
        .arg("--progress-template")
        .arg(format!(
            "download:{PROGRESS} %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s"
        ))
        .arg("-o")
        .arg(dir.join("%(id)s.%(ext)s"));
    if let Some(ffmpeg) = &config.ffmpeg {
        command.arg("--ffmpeg-location").arg(ffmpeg);
    }
    if let Some(browser) = &config.cookies_from_browser {
        command.args(["--cookies-from-browser", browser]);
    }
    command.arg("--").arg(&job.url);
    command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    let mut child = command.spawn().with_context(|| format!("running {} (is it installed?)", config.yt_dlp().display()))?;

    let mut stderr = child.stderr.take().expect("piped stderr");
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = stderr.read_to_string(&mut text).await;
        text
    });
    app.downloads.update(job.id, |d| d.state = DownloadState::Downloading);
    app.publish_downloads();
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let mut last_publish = std::time::Instant::now();
    while let Some(line) = lines.next_line().await? {
        if let Some((progress, speed)) = parse_progress(&line) {
            app.downloads.update(job.id, |d| {
                d.progress = progress;
                d.speed = speed;
            });
            if last_publish.elapsed().as_millis() >= 250 {
                app.publish_downloads();
                last_publish = std::time::Instant::now();
            }
        }
    }
    let status = child.wait().await?;
    let errors = errors.await.unwrap_or_default();
    if !status.success() {
        tracing::warn!("yt-dlp failed on {}: {}", job.url, last_error(&errors));
        bail!("{}", yt_dlp_error(&errors, config.cookies_from_browser.as_deref()));
    }
    let added = add_downloaded(app, &dir, &job.url)?;
    Ok(added)
}

/// Moves the downloaded file into the music directory and adds it to the library.
fn add_downloaded(app: &Arc<App>, dir: &Path, url: &str) -> Result<Outcome> {
    let mut audio = None;
    let mut info = None;
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_owned();
        if name.ends_with(".info.json") {
            info = Some(path);
        } else if !name.ends_with(".part") && !name.ends_with(".json") && !name.ends_with(".ytdl") {
            audio = Some(path);
        }
    }
    let audio = audio.context("yt-dlp finished but left no audio file")?;
    let info: Info = match info {
        Some(path) => serde_json::from_slice(&std::fs::read(&path)?).context("reading yt-dlp's info file")?,
        None => Info::default(),
    };
    let id = song_id(&info, &audio);
    if let Some(song) = app.lib.lock().expect("library lock").song(&id).cloned() {
        return Ok(Outcome::Known(song));
    }
    // Direct file links carry no page metadata, but the file may be tagged.
    let generic = info.extractor_key.as_deref() == Some("Generic");
    let ffprobe = app.config.lock().expect("config lock").ffprobe();
    let tags = if generic { crate::analyzer::probe(&ffprobe, &audio) } else { None };
    let page_title = info.title.as_deref().unwrap_or("Untitled");
    let page_title = if generic { page_title.replace('_', " ") } else { page_title.to_owned() };
    let (artist, title) = match tags.as_ref().and_then(|t| t.title.as_deref()) {
        Some(tag) => song_name(Raw { title: tag, track: Some(tag), artist: tags.as_ref().and_then(|t| t.artist.as_deref()), uploader: None }),
        None => song_name(Raw {
            title: &page_title,
            track: info.track.as_deref(),
            artist: info.artist.as_deref().or(info.creator.as_deref()),
            uploader: info.uploader.as_deref().or(info.channel.as_deref()),
        }),
    };
    let ext = audio.extension().and_then(|e| e.to_str()).unwrap_or("audio");
    let stem = if artist.is_empty() { title.clone() } else { format!("{artist} - {title}") };
    let file = unique_name(&app.music_dir, &format!("{} [{id}]", sanitize(&stem)), ext);
    move_file(&audio, &app.music_dir.join(&file))?;
    let song = Song {
        id: id.clone(),
        title,
        artist,
        duration_ms: info.duration.map_or(0, |d| (d * 1000.0) as u64),
        file,
        source: Some(info.webpage_url.unwrap_or_else(|| url.to_owned())),
        added: now(),
        analysis: None,
    };
    {
        let mut lib = app.lib.lock().expect("library lock");
        lib.add_song(song.clone());
        lib.set_state(&id, SongState::Analyzing);
    }
    app.publish(Event::Library);
    app.publish(Event::Playlists);
    app.library_changed();
    Ok(Outcome::Added(song))
}

async fn expand_playlist(app: &Arc<App>, url: &str) -> Result<Outcome> {
    let config = app.config.lock().expect("config lock").clone();
    let output = Command::new(config.yt_dlp())
        .args(["--flat-playlist", "-J", "--ignore-config", "--no-warnings"])
        .args(config.cookies_from_browser.as_deref().map(|b| ["--cookies-from-browser", b]).into_iter().flatten())
        .args(["--", url])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("running {} (is it installed?)", config.yt_dlp().display()))?;
    if !output.status.success() {
        let errors = String::from_utf8_lossy(&output.stderr);
        tracing::warn!("yt-dlp failed on {url}: {}", last_error(&errors));
        bail!("{}", yt_dlp_error(&errors, config.cookies_from_browser.as_deref()));
    }
    let list: PlaylistInfo = serde_json::from_slice(&output.stdout).context("reading the playlist")?;
    let mut count = 0;
    for entry in list.entries {
        let Some(link) = entry.url.or(entry.webpage_url).or_else(|| entry.id.map(|id| format!("https://www.youtube.com/watch?v={id}")))
        else {
            continue;
        };
        app.downloads.add(&link, entry.title);
        count += 1;
    }
    if count == 0 {
        bail!("that playlist has no songs vimj can reach");
    }
    Ok(Outcome::Expanded { title: list.title.unwrap_or_else(|| "Playlist".into()), count })
}

#[derive(Debug, Default, Deserialize)]
struct Info {
    id: Option<String>,
    extractor_key: Option<String>,
    title: Option<String>,
    track: Option<String>,
    artist: Option<String>,
    creator: Option<String>,
    uploader: Option<String>,
    channel: Option<String>,
    duration: Option<f64>,
    webpage_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PlaylistInfo {
    title: Option<String>,
    #[serde(default)]
    entries: Vec<PlaylistEntry>,
}

#[derive(Debug, Deserialize)]
struct PlaylistEntry {
    id: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    title: Option<String>,
}

/// YouTube video ids as they are; other sites prefixed with their name.
fn song_id(info: &Info, file: &Path) -> String {
    let raw = info.id.clone().unwrap_or_else(|| file.file_stem().and_then(|s| s.to_str()).unwrap_or("song").to_owned());
    let clean: String = raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
    match info.extractor_key.as_deref() {
        None | Some("Youtube") => clean,
        Some(site) => format!("{}-{clean}", site.to_lowercase()),
    }
}

/// Video id from the common YouTube link shapes.
fn youtube_id(url: &str) -> Option<String> {
    let valid = |id: &str| id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let after = |marker: &str| url.split_once(marker).map(|(_, rest)| rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect::<String>());
    if !url.contains("youtube.com") && !url.contains("youtu.be") {
        return None;
    }
    ["v=", "youtu.be/", "/shorts/", "/embed/", "/live/"].iter().filter_map(|m| after(m)).find(|id| valid(id))
}

/// Links to a playlist rather than to one video in it.
fn is_playlist(url: &str) -> bool {
    (url.contains("list=") && !url.contains("v=") && !url.contains("youtu.be/"))
        || url.contains("/playlist?")
        || url.contains("/sets/")
        || url.contains("/album/")
}

fn parse_progress(line: &str) -> Option<(f32, Option<f64>)> {
    let rest = line.strip_prefix(PROGRESS)?.trim();
    let fields: Vec<Option<f64>> = rest.split_whitespace().map(|f| f.parse().ok()).collect();
    let downloaded = fields.first().copied().flatten()?;
    let total = fields.get(1).copied().flatten().or(fields.get(2).copied().flatten());
    let progress = total.filter(|t| *t > 0.0).map_or(0.0, |t| (downloaded / t).clamp(0.0, 1.0)) as f32;
    Some((progress, fields.get(3).copied().flatten()))
}

/// yt-dlp's own last error line, for the log.
fn last_error(stderr: &str) -> &str {
    stderr
        .lines()
        .rev()
        .find(|l| l.contains("ERROR"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("no output")
        .trim()
}

/// A short explanation of why yt-dlp failed. `cookies` is the browser whose
/// login downloads already use, if any.
fn yt_dlp_error(stderr: &str, cookies: Option<&str>) -> String {
    let bot_check = stderr.contains("confirm you’re not a bot") || stderr.contains("confirm you're not a bot");
    let blocked = stderr.contains("HTTP Error 429") || stderr.contains("google.com/sorry");
    if blocked && (bot_check || stderr.contains("youtube")) {
        return "YouTube is blocking this internet connection for unusual traffic (HTTP 429). Open a YouTube \
                video in your browser and pass the check if one appears, or wait an hour or so, or try \
                another network; then add the link again"
            .into();
    }
    if bot_check {
        return match cookies {
            Some(browser) => format!(
                "YouTube wants proof you're not a bot even with {browser}'s login. Open youtube.com in \
                 {browser}, make sure you're signed in, then add the link again"
            ),
            None => "YouTube wants proof you're not a bot. Run `:cookies firefox` (or chrome, brave…) so \
                     downloads use your browser's YouTube login"
                .into(),
        };
    }
    let line = last_error(stderr);
    let line = line.strip_prefix("ERROR:").unwrap_or(line).trim();
    // "[youtube] abc: Video unavailable" reads better without the prefix.
    let line = match line.split_once("]: ").or_else(|| line.split_once("] ")) {
        Some((tag, rest)) if tag.starts_with('[') => rest.split_once(": ").map_or(rest, |(_, msg)| msg),
        _ => line,
    };
    if line.is_empty() || line == "no output" { "yt-dlp failed".into() } else { line.to_owned() }
}

/// File-name safe, and not too long.
pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') || c.is_control() { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').to_owned();
    let mut out = String::new();
    for c in trimmed.chars() {
        if out.len() + c.len_utf8() > 150 {
            break;
        }
        out.push(c);
    }
    if out.is_empty() { "song".into() } else { out }
}

fn unique_name(dir: &Path, stem: &str, ext: &str) -> String {
    let mut name = format!("{stem}.{ext}");
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("{stem} ({n}).{ext}");
        n += 1;
    }
    name
}

fn move_file(from: &Path, to: &PathBuf) -> Result<()> {
    if std::fs::rename(from, to).is_err() {
        // Across file systems.
        std::fs::copy(from, to).with_context(|| format!("copying into {}", to.display()))?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

pub fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_youtube_links() {
        assert_eq!(youtube_id("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=10"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(youtube_id("https://youtu.be/dQw4w9WgXcQ?si=x"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(youtube_id("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RD"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(youtube_id("https://www.youtube.com/shorts/dQw4w9WgXcQ"), Some("dQw4w9WgXcQ".into()));
        assert_eq!(youtube_id("https://soundcloud.com/a/b"), None);
        assert!(is_playlist("https://www.youtube.com/playlist?list=PL123"));
        assert!(is_playlist("https://music.youtube.com/playlist?list=OLAK5"));
        assert!(!is_playlist("https://www.youtube.com/watch?v=dQw4w9WgXcQ&list=RD"));
    }

    #[test]
    fn parses_progress_lines() {
        assert_eq!(parse_progress("vimj-progress 500 1000 NA 2048.5"), Some((0.5, Some(2048.5))));
        assert_eq!(parse_progress("vimj-progress 500 NA 2000 NA"), Some((0.25, None)));
        assert_eq!(parse_progress("[download] Destination: x"), None);
    }

    #[test]
    fn explains_failures_briefly() {
        let stderr = "WARNING: something\nERROR: [youtube] abc123: Video unavailable. This video is private\n";
        assert_eq!(yt_dlp_error(stderr, None), "Video unavailable. This video is private");
        assert_eq!(yt_dlp_error("ERROR: Unsupported URL: https://x", None), "Unsupported URL: https://x");
        let bot = "ERROR: [youtube] x: Sign in to confirm you’re not a bot. Use --cookies";
        assert!(yt_dlp_error(bot, None).contains(":cookies"));
        assert!(yt_dlp_error(bot, Some("firefox")).contains("Open youtube.com in firefox"));
        let blocked = "WARNING: [youtube] x: Unable to download webpage: HTTP Error 429: Too Many Requests\n".to_owned() + bot;
        assert!(yt_dlp_error(&blocked, Some("firefox")).contains("blocking this internet connection"));
    }

    #[test]
    fn names_files_safely() {
        assert_eq!(sanitize("AC/DC - T.N.T."), "AC_DC - T.N.T");
        assert_eq!(sanitize("  "), "song");
        let info = Info { id: Some("abc".into()), extractor_key: Some("Soundcloud".into()), ..Info::default() };
        assert_eq!(song_id(&info, Path::new("x")), "soundcloud-abc");
    }
}
