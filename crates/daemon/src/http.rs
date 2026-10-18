//! The local HTTP API frontends talk to. See `vimj_core::protocol`.

use crate::app::App;
use crate::library;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use vimj_audio::engine::Command;
use vimj_core::Transition;
use vimj_core::protocol::*;
use futures_util::{Stream, StreamExt};
use std::convert::Infallible;
use std::sync::Arc;
use tokio_stream::wrappers::BroadcastStream;

type Shared = State<Arc<App>>;
type ApiResult<T> = Result<Json<T>, ApiError>;

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(ErrorBody { error: self.1 })).into_response()
    }
}

impl From<library::Error> for ApiError {
    fn from(e: library::Error) -> ApiError {
        let status = match e {
            library::Error::NotFound(_) => StatusCode::NOT_FOUND,
            library::Error::Invalid(_) => StatusCode::BAD_REQUEST,
        };
        ApiError(status, e.to_string())
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> ApiError {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

fn bad_request(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, message.into())
}

fn internal(e: impl std::fmt::Display) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/status", get(status))
        .route("/api/config", patch(update_config))
        .route("/api/playlists", get(playlists).post(create_playlist))
        .route("/api/playlists/{id}", get(playlist).patch(update_playlist).delete(delete_playlist))
        .route("/api/playlists/{id}/songs", post(add_songs))
        .route("/api/playlists/{id}/remove", post(remove_songs))
        .route("/api/playlists/{id}/move", post(move_songs))
        .route("/api/playlists/{id}/place-after", post(place_after))
        .route("/api/playlists/{id}/smart-reorder", post(smart_reorder))
        .route("/api/playlists/{id}/undo", post(undo))
        .route("/api/playlists/{id}/redo", post(redo))
        .route("/api/playlists/{id}/suggestions/{index}", get(suggestions))
        .route("/api/songs/{id}", get(song).patch(update_song).delete(delete_song))
        .route("/api/songs/{id}/waveform", get(waveform))
        .route("/api/rescan", post(rescan))
        .route("/api/downloads", get(downloads).post(download))
        .route("/api/downloads/finished", delete(clear_downloads))
        .route("/api/downloads/{id}", delete(dismiss_download))
        .route("/api/transitions/{from}/{to}", get(transition).put(set_transition).delete(reset_transition))
        .route("/api/player", get(player))
        .route("/api/player/play", post(play))
        .route("/api/player/preview", post(preview))
        .route("/api/player/toggle", post(toggle))
        .route("/api/player/stop", post(stop))
        .route("/api/player/next", post(next))
        .route("/api/player/previous", post(previous))
        .route("/api/player/seek", post(seek))
        .route("/api/player/volume", post(volume))
        .route("/api/events", get(events))
        .route("/api/shutdown", post(shutdown))
        .with_state(app)
}

async fn status(State(app): Shared) -> Json<Status> {
    Json(app.status())
}

async fn update_config(State(app): Shared, Json(req): Json<ConfigUpdate>) -> ApiResult<Status> {
    Ok(Json(app.update_config(req)?))
}

// Playlists.

async fn playlists(State(app): Shared) -> Json<Playlists> {
    Json(app.lib.lock().expect("library lock").playlists())
}

async fn create_playlist(State(app): Shared, Json(req): Json<CreatePlaylist>) -> ApiResult<PlaylistSummary> {
    let summary = app.lib.lock().expect("library lock").create_playlist(&req.name, req.songs)?;
    app.publish(Event::Playlists);
    Ok(Json(summary))
}

async fn playlist(State(app): Shared, Path(id): Path<String>) -> ApiResult<PlaylistView> {
    Ok(Json(app.lib.lock().expect("library lock").view(&id)?))
}

/// Runs a change to one playlist, then tells everyone and returns the new view.
fn change(app: &App, id: &str, f: impl FnOnce(&mut library::Library) -> library::Result<()>) -> ApiResult<PlaylistView> {
    let view = {
        let mut lib = app.lib.lock().expect("library lock");
        f(&mut lib)?;
        lib.view(id)?
    };
    app.library_changed();
    app.publish(Event::Playlist { id: id.to_owned() });
    app.publish(Event::Playlists);
    Ok(Json(view))
}

async fn update_playlist(State(app): Shared, Path(id): Path<String>, Json(req): Json<PlaylistUpdate>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.update_playlist(&id, req))
}

async fn delete_playlist(State(app): Shared, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    app.lib.lock().expect("library lock").delete_playlist(&id)?;
    app.publish(Event::Playlists);
    Ok(StatusCode::NO_CONTENT)
}

async fn add_songs(State(app): Shared, Path(id): Path<String>, Json(req): Json<AddSongs>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.add_songs(&id, &req.songs, req.at))
}

async fn remove_songs(State(app): Shared, Path(id): Path<String>, Json(req): Json<RemoveSongs>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.remove(&id, &req.indices))
}

async fn move_songs(State(app): Shared, Path(id): Path<String>, Json(req): Json<MoveSongs>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.move_block(&id, req.from, req.count, req.to))
}

async fn place_after(State(app): Shared, Path(id): Path<String>, Json(req): Json<PlaceAfter>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.place_after(&id, req.anchor, &req.song))
}

async fn smart_reorder(State(app): Shared, Path(id): Path<String>) -> ApiResult<PlaylistView> {
    // Reordering thousands of songs takes a moment; keep it off the async threads.
    tokio::task::spawn_blocking(move || change(&app, &id, |lib| lib.smart_reorder(&id))).await.map_err(internal)?
}

async fn undo(State(app): Shared, Path(id): Path<String>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.undo(&id))
}

async fn redo(State(app): Shared, Path(id): Path<String>) -> ApiResult<PlaylistView> {
    change(&app, &id, |lib| lib.redo(&id))
}

async fn suggestions(State(app): Shared, Path((id, index)): Path<(String, usize)>) -> ApiResult<Vec<Suggestion>> {
    Ok(Json(app.lib.lock().expect("library lock").suggestions(&id, index)?))
}

// Songs.

async fn song(State(app): Shared, Path(id): Path<String>) -> ApiResult<SongInfo> {
    Ok(Json(app.lib.lock().expect("library lock").info(&id)?))
}

async fn update_song(State(app): Shared, Path(id): Path<String>, Json(req): Json<SongUpdate>) -> ApiResult<SongInfo> {
    let info = app.lib.lock().expect("library lock").update_song(&id, req)?;
    app.publish(Event::Library);
    Ok(Json(info))
}

/// Deletes the song's file too.
async fn delete_song(State(app): Shared, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    let song = app.lib.lock().expect("library lock").remove_song(&id)?;
    let _ = std::fs::remove_file(app.music_dir.join(&song.file));
    let _ = std::fs::remove_file(app.paths.waveforms.join(format!("{id}.bin")));
    app.library_changed();
    app.publish(Event::Library);
    app.publish(Event::Playlists);
    Ok(StatusCode::NO_CONTENT)
}

async fn waveform(State(app): Shared, Path(id): Path<String>) -> Result<Response, ApiError> {
    if app.lib.lock().expect("library lock").song(&id).is_none() {
        return Err(ApiError(StatusCode::NOT_FOUND, "song not found".into()));
    }
    match tokio::fs::read(app.paths.waveforms.join(format!("{id}.bin"))).await {
        Ok(bytes) => Ok(([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()),
        Err(_) => Err(ApiError(StatusCode::NOT_FOUND, "not analysed yet".into())),
    }
}

async fn rescan(State(app): Shared) -> ApiResult<usize> {
    let added = tokio::task::spawn_blocking(move || crate::analyzer::rescan(&app)).await.map_err(internal)??;
    Ok(Json(added))
}

// Downloads.

async fn downloads(State(app): Shared) -> Json<Vec<Download>> {
    Json(app.downloads.list())
}

async fn download(State(app): Shared, Json(req): Json<DownloadRequest>) -> ApiResult<Vec<Download>> {
    let url = req.url.trim();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(bad_request("that doesn't look like a link"));
    }
    app.downloads.add(url, None);
    app.publish_downloads();
    Ok(Json(app.downloads.list()))
}

async fn dismiss_download(State(app): Shared, Path(id): Path<u64>) -> ApiResult<Vec<Download>> {
    if !app.downloads.dismiss(id) {
        return Err(ApiError(StatusCode::NOT_FOUND, "download not found".into()));
    }
    app.publish_downloads();
    Ok(Json(app.downloads.list()))
}

async fn clear_downloads(State(app): Shared) -> Json<Vec<Download>> {
    app.downloads.clear_finished();
    app.publish_downloads();
    Json(app.downloads.list())
}

// Transitions.

async fn transition(State(app): Shared, Path((from, to)): Path<(String, String)>) -> ApiResult<TransitionDetail> {
    Ok(Json(app.lib.lock().expect("library lock").transition_detail(&from, &to)?))
}

fn transition_changed(app: &App) {
    app.library_changed();
    // Every playlist holding the pair shows it; let frontends reload theirs.
    let ids: Vec<String> = {
        let playlists = app.lib.lock().expect("library lock").playlists();
        std::iter::once(playlists.library.id).chain(playlists.playlists.into_iter().map(|p| p.id)).collect()
    };
    for id in ids {
        app.publish(Event::Playlist { id });
    }
}

async fn set_transition(
    State(app): Shared,
    Path((from, to)): Path<(String, String)>,
    Json(t): Json<Transition>,
) -> ApiResult<TransitionDetail> {
    let detail = app.lib.lock().expect("library lock").set_transition(&from, &to, t)?;
    transition_changed(&app);
    Ok(Json(detail))
}

async fn reset_transition(State(app): Shared, Path((from, to)): Path<(String, String)>) -> ApiResult<TransitionDetail> {
    let detail = app.lib.lock().expect("library lock").reset_transition(&from, &to)?;
    transition_changed(&app);
    Ok(Json(detail))
}

// Player.

async fn player(State(app): Shared) -> Json<PlayerState> {
    Json(app.player_state())
}

async fn play(State(app): Shared, Json(req): Json<PlayRequest>) -> ApiResult<PlayerState> {
    {
        let lib = app.lib.lock().expect("library lock");
        app.playback.play(&lib, &app.music_dir, &req.context, req.index, req.position_ms)?;
    }
    Ok(Json(app.player_state()))
}

async fn preview(State(app): Shared, Json(req): Json<PreviewRequest>) -> ApiResult<PlayerState> {
    {
        let lib = app.lib.lock().expect("library lock");
        app.playback.preview(&lib, &app.music_dir, &req.from, &req.to, req.lead_in_ms)?;
    }
    Ok(Json(app.player_state()))
}

fn player_command(app: &App, command: Command) -> Json<PlayerState> {
    app.playback.send(command);
    Json(app.player_state())
}

async fn toggle(State(app): Shared) -> Json<PlayerState> {
    app.playback.toggle();
    Json(app.player_state())
}

async fn stop(State(app): Shared) -> Json<PlayerState> {
    player_command(&app, Command::Stop)
}

async fn next(State(app): Shared) -> Json<PlayerState> {
    player_command(&app, Command::Next)
}

async fn previous(State(app): Shared) -> Json<PlayerState> {
    player_command(&app, Command::Previous)
}

async fn seek(State(app): Shared, Json(req): Json<SeekRequest>) -> Json<PlayerState> {
    player_command(&app, Command::Seek(req.position_ms.max(0.0)))
}

async fn volume(State(app): Shared, Json(req): Json<VolumeRequest>) -> Json<PlayerState> {
    let volume = req.volume.min(100);
    app.save_volume(volume);
    player_command(&app, Command::Volume(f32::from(volume) / 100.0))
}

async fn events(State(app): Shared) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let live = BroadcastStream::new(app.events.subscribe()).filter_map(|e| async move { e.ok() });
    let initial = vec![
        Event::Status(app.status()),
        Event::Player(app.player_state()),
        Event::Downloads { downloads: app.downloads.list() },
    ];
    let stream = futures_util::stream::iter(initial)
        .chain(live)
        .map(|e| Ok(SseEvent::default().json_data(e).unwrap_or_default()));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn shutdown(State(app): Shared) -> StatusCode {
    app.shutdown.notify_one();
    StatusCode::NO_CONTENT
}
