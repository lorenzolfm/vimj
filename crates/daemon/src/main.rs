//! vimjd: keeps the music library, downloads songs with yt-dlp, analyses
//! them and plays mixes on the sound card. Frontends talk to it over HTTP on
//! 127.0.0.1.

mod analyzer;
mod app;
mod config;
mod downloads;
mod http;
mod library;
mod playback;

use anyhow::{Context, Result, bail};
use app::App;
use vimj_audio::player::Output;
use vimj_core::protocol::DEFAULT_PORT;
use tracing_subscriber::EnvFilter;

struct Args {
    port: u16,
    output: Output,
}

fn args() -> Result<Args> {
    let mut args = Args { port: DEFAULT_PORT, output: Output::Device };
    if let Ok(port) = std::env::var("VIMJ_PORT") {
        args.port = port.parse().context("VIMJ_PORT is not a port number")?;
    }
    if std::env::var("VIMJ_AUDIO").is_ok_and(|v| v == "null") {
        args.output = Output::Null;
    }
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" | "-p" => args.port = it.next().context("--port needs a value")?.parse()?,
            "--silent" => args.output = Output::Null,
            "--help" | "-h" => {
                println!(
                    "usage: vimjd [--port PORT] [--silent]\n\n\
                     The vimj daemon. Start the `vimj` TUI to use it.\n\
                     --silent plays into nothing instead of the sound card."
                );
                std::process::exit(0);
            }
            other => bail!("unexpected argument {other:?}"),
        }
    }
    Ok(args)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("vimjd=info")))
        .with_writer(std::io::stderr)
        .init();
    let args = args()?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", args.port))
        .await
        .with_context(|| format!("binding 127.0.0.1:{}; is vimjd already running?", args.port))?;
    let app = App::new(config::Paths::new()?, args.output)?;
    tracing::info!("songs live in {}", app.music_dir.display());
    match app.playback.device() {
        Ok(name) => tracing::info!("playing on {name}"),
        Err(e) => tracing::warn!("no sound output ({e}); playing silently"),
    }

    {
        let app = app.clone();
        tokio::spawn(async move {
            app.probe_tools().await;
            let scan = app.clone();
            match tokio::task::spawn_blocking(move || analyzer::rescan(&scan)).await {
                Ok(Ok(n)) if n > 0 => tracing::info!("found {n} new songs in the music directory"),
                Ok(Err(e)) => tracing::warn!("scanning the music directory: {e:#}"),
                _ => {}
            }
            analyzer::run(app).await;
        });
    }
    tokio::spawn(downloads::run(app.clone()));

    tracing::info!("listening on http://127.0.0.1:{}", args.port);
    let shutdown = app.clone();
    axum::serve(listener, http::router(app))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = shutdown.shutdown.notified() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
            tracing::info!("shutting down");
        })
        .await?;
    Ok(())
}
