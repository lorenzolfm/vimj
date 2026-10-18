//! HTTP client for the daemon, plus the event stream listener.

use anyhow::{Result, anyhow};
use vimj_core::Transition;
use vimj_core::protocol::*;
use futures_util::StreamExt;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(port: u16) -> Result<Client> {
        // Smart reorder of a big library can take a moment.
        let http = reqwest::Client::builder().timeout(Duration::from_secs(60)).build()?;
        Ok(Client { http, base: format!("http://127.0.0.1:{port}") })
    }

    async fn send<T: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            let message = serde_json::from_slice::<ErrorBody>(&bytes)
                .map(|e| e.error)
                .unwrap_or_else(|_| format!("daemon returned {status}"));
            return Err(anyhow!(message));
        }
        let body = if bytes.is_empty() { b"null".as_slice() } else { &bytes };
        Ok(serde_json::from_slice(body)?)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.get(self.url(path))).await
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: &impl Serialize) -> Result<T> {
        self.send(self.http.post(self.url(path)).json(body)).await
    }

    async fn post_empty<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.post(self.url(path))).await
    }

    async fn delete<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        self.send(self.http.delete(self.url(path))).await
    }

    pub async fn status(&self) -> Result<Status> {
        self.get("/api/status").await
    }

    pub async fn update_config(&self, update: ConfigUpdate) -> Result<Status> {
        self.send(self.http.patch(self.url("/api/config")).json(&update)).await
    }

    pub async fn playlists(&self) -> Result<Playlists> {
        self.get("/api/playlists").await
    }

    pub async fn create_playlist(&self, name: String, songs: Vec<String>) -> Result<PlaylistSummary> {
        self.post("/api/playlists", &CreatePlaylist { name, songs }).await
    }

    pub async fn playlist(&self, id: &str) -> Result<PlaylistView> {
        self.get(&format!("/api/playlists/{id}")).await
    }

    pub async fn update_playlist(&self, id: &str, update: PlaylistUpdate) -> Result<PlaylistView> {
        self.send(self.http.patch(self.url(&format!("/api/playlists/{id}"))).json(&update)).await
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/playlists/{id}")).await
    }

    /// smart-reorder, undo or redo.
    pub async fn playlist_action(&self, id: &str, action: &str) -> Result<PlaylistView> {
        self.post_empty(&format!("/api/playlists/{id}/{action}")).await
    }

    pub async fn add_songs(&self, id: &str, songs: Vec<String>, at: Option<usize>) -> Result<PlaylistView> {
        self.post(&format!("/api/playlists/{id}/songs"), &AddSongs { songs, at }).await
    }

    pub async fn remove_songs(&self, id: &str, indices: Vec<usize>) -> Result<PlaylistView> {
        self.post(&format!("/api/playlists/{id}/remove"), &RemoveSongs { indices }).await
    }

    pub async fn move_songs(&self, id: &str, from: usize, count: usize, to: usize) -> Result<PlaylistView> {
        self.post(&format!("/api/playlists/{id}/move"), &MoveSongs { from, count, to }).await
    }

    pub async fn place_after(&self, id: &str, anchor: usize, song: String) -> Result<PlaylistView> {
        self.post(&format!("/api/playlists/{id}/place-after"), &PlaceAfter { anchor, song }).await
    }

    pub async fn suggestions(&self, id: &str, index: usize) -> Result<Vec<Suggestion>> {
        self.get(&format!("/api/playlists/{id}/suggestions/{index}")).await
    }

    pub async fn update_song(&self, id: &str, update: SongUpdate) -> Result<SongInfo> {
        self.send(self.http.patch(self.url(&format!("/api/songs/{id}"))).json(&update)).await
    }

    pub async fn delete_song(&self, id: &str) -> Result<()> {
        self.delete(&format!("/api/songs/{id}")).await
    }

    pub async fn waveform(&self, id: &str) -> Result<Vec<u8>> {
        let response = self.http.get(self.url(&format!("/api/songs/{id}/waveform"))).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("no waveform"));
        }
        Ok(response.bytes().await?.to_vec())
    }

    pub async fn rescan(&self) -> Result<usize> {
        self.post_empty("/api/rescan").await
    }

    pub async fn download(&self, url: String) -> Result<Vec<Download>> {
        self.post("/api/downloads", &DownloadRequest { url }).await
    }

    pub async fn dismiss_download(&self, id: u64) -> Result<Vec<Download>> {
        self.delete(&format!("/api/downloads/{id}")).await
    }

    pub async fn clear_downloads(&self) -> Result<Vec<Download>> {
        self.delete("/api/downloads/finished").await
    }

    pub async fn transition(&self, from: &str, to: &str) -> Result<TransitionDetail> {
        self.get(&format!("/api/transitions/{from}/{to}")).await
    }

    pub async fn set_transition(&self, from: &str, to: &str, t: &Transition) -> Result<TransitionDetail> {
        self.send(self.http.put(self.url(&format!("/api/transitions/{from}/{to}"))).json(t)).await
    }

    pub async fn reset_transition(&self, from: &str, to: &str) -> Result<TransitionDetail> {
        self.delete(&format!("/api/transitions/{from}/{to}")).await
    }

    pub async fn play(&self, context: String, index: usize, position_ms: f64) -> Result<PlayerState> {
        self.post("/api/player/play", &PlayRequest { context, index, position_ms }).await
    }

    pub async fn preview(&self, from: String, to: String, lead_in_ms: f64) -> Result<PlayerState> {
        self.post("/api/player/preview", &PreviewRequest { from, to, lead_in_ms }).await
    }

    /// toggle, stop, next or previous.
    pub async fn player_action(&self, action: &str) -> Result<PlayerState> {
        self.post_empty(&format!("/api/player/{action}")).await
    }

    pub async fn seek(&self, position_ms: f64) -> Result<PlayerState> {
        self.post("/api/player/seek", &SeekRequest { position_ms }).await
    }

    pub async fn volume(&self, volume: u8) -> Result<PlayerState> {
        self.post("/api/player/volume", &VolumeRequest { volume }).await
    }

    pub async fn shutdown(&self) -> Result<()> {
        self.post_empty("/api/shutdown").await
    }

    /// Streams daemon events into `tx`, reconnecting forever. Sends
    /// `on_connect(true/false)` as the connection comes and goes.
    pub fn listen<M: Send + 'static>(&self, tx: UnboundedSender<M>, on_event: fn(Event) -> M, on_connect: fn(bool) -> M) {
        let url = self.url("/api/events");
        tokio::spawn(async move {
            // No timeout: the stream stays open.
            let http = reqwest::Client::new();
            loop {
                if let Ok(response) = http.get(&url).send().await
                    && response.status().is_success()
                {
                    let _ = tx.send(on_connect(true));
                    let mut stream = response.bytes_stream();
                    let mut buffer: Vec<u8> = Vec::new();
                    while let Some(Ok(chunk)) = stream.next().await {
                        buffer.extend_from_slice(&chunk);
                        while let Some(end) = find_blank_line(&buffer) {
                            let block: Vec<u8> = buffer.drain(..end + 2).collect();
                            if let Some(event) = parse_sse(&block)
                                && tx.send(on_event(event)).is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                if tx.send(on_connect(false)).is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    }
}

fn find_blank_line(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\n\n")
}

fn parse_sse(block: &[u8]) -> Option<Event> {
    let text = std::str::from_utf8(block).ok()?;
    let data: Vec<&str> = text.lines().filter_map(|l| l.strip_prefix("data:")).map(str::trim_start).collect();
    if data.is_empty() {
        return None;
    }
    serde_json::from_str(&data.join("\n")).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_server_sent_events() {
        let block = b"data: {\"type\":\"playlist\",\"id\":\"p1\"}\n\n";
        assert_eq!(parse_sse(block), Some(Event::Playlist { id: "p1".into() }));
        assert_eq!(parse_sse(b":keep-alive\n\n"), None);
        assert_eq!(find_blank_line(b"data: x\n\ndata: y"), Some(7));
    }
}
