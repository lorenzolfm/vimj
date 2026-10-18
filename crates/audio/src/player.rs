//! Runs the engine for real: a render thread fills a ring buffer that the
//! sound card drains, and a loader thread decodes songs the engine asks for.

use crate::engine::{Command, Engine, Request, Snapshot};
use crate::pcm;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Frames rendered at a time.
const BLOCK: usize = 512;
/// Audio buffered ahead of the sound card: about 85 ms at 48 kHz.
const RING_FRAMES: usize = 4_096;
/// How often the snapshot is refreshed while playing.
const PUBLISH_EVERY: Duration = Duration::from_millis(50);
/// Sample rate when there is no sound card.
const NULL_RATE: u32 = 48_000;

pub enum Output {
    /// The system's default sound output.
    Device,
    /// Plays into nothing at real-time speed; for tests and machines without sound.
    Null,
}

pub struct Player {
    commands: Sender<Message>,
    snapshot: Arc<Mutex<Snapshot>>,
    rate: u32,
    /// Output device name, or why none could be opened.
    pub device: Result<String, String>,
}

enum Message {
    Command(Command),
    Shutdown,
}

impl Player {
    /// Starts playback threads. `on_change` runs on the render thread with
    /// each new snapshot; keep it cheap.
    pub fn start(output: Output, ffmpeg: PathBuf, on_change: impl Fn(&Snapshot) + Send + 'static) -> Player {
        let (ring_in, ring_out) = rtrb::RingBuffer::<f32>::new(RING_FRAMES * 2);
        let (rate, device) = match output {
            Output::Device => match open_device(ring_out) {
                Ok((rate, name)) => (rate, Ok(name)),
                Err((e, ring_out)) => {
                    null_output(ring_out, NULL_RATE);
                    (NULL_RATE, Err(e))
                }
            },
            Output::Null => {
                null_output(ring_out, NULL_RATE);
                (NULL_RATE, Ok("no output (silent)".into()))
            }
        };
        let (commands, inbox) = mpsc::channel();
        let (loads, load_inbox) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        {
            let commands = commands.clone();
            std::thread::Builder::new()
                .name("dejay-loader".into())
                .spawn(move || loader(load_inbox, commands, ffmpeg, rate))
                .expect("spawning the loader thread");
        }
        {
            let snapshot = snapshot.clone();
            std::thread::Builder::new()
                .name("dejay-render".into())
                .spawn(move || render(Engine::new(rate), inbox, loads, ring_in, snapshot, on_change))
                .expect("spawning the render thread");
        }
        Player { commands, snapshot, rate, device }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(Message::Command(command));
    }

    pub fn snapshot(&self) -> Snapshot {
        self.snapshot.lock().expect("snapshot lock").clone()
    }

    pub fn rate(&self) -> u32 {
        self.rate
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.commands.send(Message::Shutdown);
    }
}

fn render(
    mut engine: Engine,
    inbox: Receiver<Message>,
    loads: Sender<Request>,
    mut ring: rtrb::Producer<f32>,
    snapshot: Arc<Mutex<Snapshot>>,
    on_change: impl Fn(&Snapshot),
) {
    let mut block = vec![0.0f32; BLOCK * 2];
    let mut published = Instant::now();
    let mut last = Snapshot::default();
    loop {
        // Wait for room in the ring, waking early for commands.
        let wait = if ring.slots() >= BLOCK * 2 { Duration::ZERO } else { Duration::from_millis(3) };
        let mut changed = false;
        match inbox.recv_timeout(wait) {
            Ok(Message::Command(c)) => {
                engine.command(c);
                changed = true;
                while let Ok(m) = inbox.try_recv() {
                    match m {
                        Message::Command(c) => engine.command(c),
                        Message::Shutdown => return,
                    }
                }
            }
            Ok(Message::Shutdown) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        for request in engine.take_requests() {
            let _ = loads.send(request);
        }
        while ring.slots() >= BLOCK * 2 {
            engine.render(&mut block);
            if let Ok(chunk) = ring.write_chunk_uninit(BLOCK * 2) {
                chunk.fill_from_iter(block.iter().copied());
            }
        }
        if changed || published.elapsed() >= PUBLISH_EVERY {
            let buffered = (RING_FRAMES * 2 - ring.slots()) / 2;
            let now = engine.snapshot(buffered);
            if now != last {
                *snapshot.lock().expect("snapshot lock") = now.clone();
                on_change(&now);
                last = now;
            }
            published = Instant::now();
        }
    }
}

fn loader(inbox: Receiver<Request>, engine: Sender<Message>, ffmpeg: PathBuf, rate: u32) {
    for request in inbox {
        let Request::Load { id, path } = request;
        let command = match pcm::decode(&ffmpeg, &path, rate) {
            Ok(pcm) => Command::Loaded { id, pcm: Arc::new(pcm) },
            Err(e) => Command::LoadFailed { id, error: format!("{e:#}") },
        };
        if engine.send(Message::Command(command)).is_err() {
            return;
        }
    }
}

type RingOut = rtrb::Consumer<f32>;

/// Opens the default output and starts draining the ring into it. The
/// stream lives on its own thread, since it may not be sendable.
fn open_device(ring: RingOut) -> Result<(u32, String), (String, RingOut)> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(u32, String), String>>();
    let (back_tx, back_rx) = mpsc::channel::<RingOut>();
    std::thread::Builder::new()
        .name("dejay-output".into())
        .spawn(move || {
            let host = cpal::default_host();
            let Some(device) = host.default_output_device() else {
                let _ = ready_tx.send(Err("no sound output device".into()));
                let _ = back_tx.send(ring);
                return;
            };
            let name = device.description().map(|d| d.name().to_string()).unwrap_or_else(|_| "sound output".into());
            let config = match device.default_output_config() {
                Ok(c) => c,
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{name}: {e}")));
                    let _ = back_tx.send(ring);
                    return;
                }
            };
            let rate = config.sample_rate();
            let format = config.sample_format();
            let config: cpal::StreamConfig = config.into();
            let stream = match format {
                cpal::SampleFormat::F32 => build::<f32>(&device, config, ring),
                cpal::SampleFormat::I16 => build::<i16>(&device, config, ring),
                cpal::SampleFormat::I32 => build::<i32>(&device, config, ring),
                cpal::SampleFormat::U16 => build::<u16>(&device, config, ring),
                other => Err(format!("unsupported sample format {other}")),
            };
            match stream.and_then(|s| s.play().map(|()| s).map_err(|e| e.to_string())) {
                Ok(stream) => {
                    let _ = ready_tx.send(Ok((rate, name)));
                    // Keep the stream alive for the life of the process.
                    loop {
                        std::thread::park();
                        let _ = &stream;
                    }
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("{name}: {e}")));
                }
            }
        })
        .expect("spawning the output thread");
    match ready_rx.recv() {
        Ok(Ok(opened)) => Ok(opened),
        Ok(Err(e)) => match back_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(ring) => Err((e, ring)),
            // The ring went into a stream that failed to start; make a fresh one.
            Err(_) => Err((e, rtrb::RingBuffer::new(RING_FRAMES * 2).1)),
        },
        Err(_) => Err(("the sound output thread stopped".into(), rtrb::RingBuffer::new(RING_FRAMES * 2).1)),
    }
}

fn build<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    mut ring: RingOut,
) -> Result<cpal::Stream, String> {
    let channels = usize::from(config.channels);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    let (l, r) = match ring.read_chunk(2) {
                        Ok(chunk) => {
                            let (a, b) = chunk.as_slices();
                            let pair = if a.len() == 2 { (a[0], a[1]) } else { (a[0], b[0]) };
                            chunk.commit_all();
                            pair
                        }
                        Err(_) => (0.0, 0.0),
                    };
                    for (c, sample) in frame.iter_mut().enumerate() {
                        let v = match (c, channels) {
                            (_, 1) => (l + r) * 0.5,
                            (0, _) => l,
                            (1, _) => r,
                            _ => 0.0,
                        };
                        *sample = T::from_sample(v);
                    }
                }
            },
            |e| eprintln!("sound output: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

/// Drains the ring at real-time speed into nothing.
fn null_output(mut ring: RingOut, rate: u32) {
    std::thread::Builder::new()
        .name("dejay-null-output".into())
        .spawn(move || {
            let start = Instant::now();
            let mut consumed: u64 = 0;
            loop {
                std::thread::sleep(Duration::from_millis(5));
                let due = (start.elapsed().as_secs_f64() * f64::from(rate)) as u64;
                let frames = (due - consumed).min((ring.slots() / 2) as u64) as usize;
                if let Ok(chunk) = ring.read_chunk(frames * 2) {
                    chunk.commit_all();
                }
                // Count time as passing even when nothing was buffered.
                consumed = due;
            }
        })
        .expect("spawning the null output thread");
}
