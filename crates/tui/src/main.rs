//! vimj: terminal frontend. Starts `vimjd` in the background if it is not
//! already running, then talks to it over HTTP.

mod app;
mod client;
mod editor;
mod mouse;
mod theme;
mod ui;
mod wave;

use anyhow::{Context, Result, bail};
use app::{App, Msg};
use client::{Answer, Client};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, EventStream};
use crossterm::execute;
use vimj_core::protocol::{DEFAULT_PORT, PROTOCOL};
use futures_util::StreamExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(100);
const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(5);

struct Args {
    port: u16,
    spawn: bool,
}

fn args() -> Result<Args> {
    let mut args = Args { port: DEFAULT_PORT, spawn: true };
    if let Ok(port) = std::env::var("VIMJ_PORT") {
        args.port = port.parse().context("VIMJ_PORT is not a port number")?;
    }
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--port" | "-p" => args.port = it.next().context("--port needs a value")?.parse()?,
            "--no-spawn" => args.spawn = false,
            "--help" | "-h" => {
                println!(
                    "usage: vimj [--port PORT] [--no-spawn]\n\n\
                     Terminal frontend for vimjd. Starts the daemon if it is not running,\n\
                     unless --no-spawn is given. Press ? inside for keys."
                );
                std::process::exit(0);
            }
            other => bail!("unexpected argument {other:?}"),
        }
    }
    Ok(args)
}

fn log_path() -> PathBuf {
    let dir = dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("vimj");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("vimjd.log")
}

/// Makes sure this build's vimjd answers on the port, starting one if
/// nothing does. Anything else there is an error, not a daemon to talk to.
async fn ensure_daemon(client: &Client, args: &Args) -> Result<()> {
    let port = args.port;
    match client.identify().await {
        Answer::Vimjd => return Ok(()),
        Answer::Nobody => {}
        Answer::OtherBuild(protocol) => bail!(
            "the vimjd on port {port} is from another vimj build (protocol {protocol}; this one speaks {PROTOCOL}). \
             Stop it with `pkill vimjd`, then start vimj again"
        ),
        Answer::Stranger => bail!(
            "something on port {port} answers, but not as a vimjd this vimj can talk to: an older vimjd, or another \
             program. Stop it, or use another port with --port or VIMJ_PORT"
        ),
    }
    if !args.spawn {
        bail!("vimjd is not running on port {}", args.port);
    }
    let sibling = std::env::current_exe()?.with_file_name("vimjd");
    let program = if sibling.exists() { sibling } else { PathBuf::from("vimjd") };
    let log_path = log_path();
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let mut command = Command::new(&program);
    command.arg("--port").arg(args.port.to_string()).stdin(Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own process group, so Ctrl-C in the terminal leaves it running.
        command.process_group(0);
    }
    command.spawn().with_context(|| format!("starting {}", program.display()))?;
    let deadline = tokio::time::Instant::now() + DAEMON_START_TIMEOUT;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if client.identify().await == Answer::Vimjd {
            return Ok(());
        }
    }
    bail!("vimjd did not start; see {}", log_path.display())
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = args()?;
    let client = Client::new(args.port)?;
    ensure_daemon(&client, &args).await?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    client.listen(tx.clone(), Msg::Event, Msg::Connected);
    let mut app = App::new(client, tx);
    app.start();

    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste)?;
    let result = run(&mut terminal, &mut app, &mut rx).await;
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    app.finish().await;
    result
}

async fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<Msg>) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(TICK);
    while !app.quit {
        terminal.draw(|frame| ui::draw(frame, app))?;
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Paste(text))) => app.on_paste(&text),
                Some(Ok(event)) => app.on_terminal(event),
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(msg) = rx.recv() => app.on_msg(msg),
            _ = tick.tick() => app.on_tick(),
        }
    }
    Ok(())
}
