//! Renders the automatic transition between two songs to a WAV file:
//! `cargo run --release --example render -- FROM TO OUT.wav [PRESET] [BARS]`
//!
//! Plays 8 seconds of FROM before the window and 8 seconds of TO after it.

use vimj_audio::engine::{Command, Engine, Entry, Plan, normalising_gain};
use vimj_audio::{analysis, pcm};
use vimj_core::compat::compat;
use vimj_core::transition::{Bars, Preset, Side, auto_transition};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const RATE: u32 = 48_000;
const AROUND_MS: f64 = 8_000.0;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [from, to, out, rest @ ..] = args.as_slice() else {
        anyhow::bail!("usage: render FROM TO OUT.wav [PRESET] [BARS]");
    };
    let load = |p: &str| -> anyhow::Result<(Arc<pcm::Pcm>, vimj_core::Analysis)> {
        let decoded = pcm::decode(Path::new("ffmpeg"), Path::new(p), RATE)?;
        let a = analysis::analyze(&decoded).analysis;
        Ok((Arc::new(decoded), a))
    };
    let (a_pcm, a) = load(from)?;
    let (b_pcm, b) = load(to)?;
    let side = |pcm: &pcm::Pcm, analysis| Side { duration_ms: pcm.duration_ms(), analysis: Some(analysis) };
    let mut t = auto_transition(side(&a_pcm, &a), side(&b_pcm, &b), &compat(Some(&a), Some(&b)));
    if let Some(name) = rest.first() {
        let preset = Preset::ALL.into_iter().find(|p| p.label().eq_ignore_ascii_case(name)).ok_or_else(|| anyhow::anyhow!("no preset {name}"))?;
        t.set_preset(preset);
    }
    if let Some(bars) = rest.get(1) {
        t.bars = Bars::try_from(bars.parse::<u32>()?).map_err(anyhow::Error::msg)?;
        t.clamp(side(&a_pcm, &a), side(&b_pcm, &b));
    }
    let timing = t.timing(a.grid.as_ref(), b.grid.as_ref());
    eprintln!("from {:?} key {:?}", a.grid, a.key.map(|k| k.name()));
    eprintln!("to   {:?} key {:?}", b.grid, b.key.map(|k| k.name()));
    eprintln!("{t:?}");
    eprintln!("{timing:?}");

    let entry = |id: &str, analysis: &vimj_core::Analysis, next| Entry {
        id: id.into(),
        path: PathBuf::from(id),
        grid: analysis.grid,
        gain_db: normalising_gain(Some(analysis.loudness)),
        next,
    };
    let mut engine = Engine::new(RATE);
    engine.command(Command::Volume(1.0));
    engine.command(Command::SetPlan(Plan { entries: vec![entry("a", &a, Some(t.clone())), entry("b", &b, None)], preview: None }));
    engine.command(Command::Loaded { id: "a".into(), pcm: a_pcm });
    engine.command(Command::Loaded { id: "b".into(), pcm: b_pcm });
    engine.command(Command::Play { index: 0, position_ms: (t.out_ms - AROUND_MS).max(0.0) });

    let seconds = (AROUND_MS * 2.0 + timing.duration_ms) / 1000.0;
    let frames = (seconds * f64::from(RATE)) as usize;
    let mut samples = vec![0.0f32; frames * 2];
    let started = std::time::Instant::now();
    for block in samples.chunks_mut(1_024) {
        engine.render(block);
    }
    eprintln!("rendered {seconds:.1} s of mix in {:.2} s", started.elapsed().as_secs_f64());
    write_wav(Path::new(out), &samples)?;
    eprintln!("wrote {seconds:.1} s to {out}; the window runs {:.1}..{:.1} s", AROUND_MS / 1000.0, (AROUND_MS + timing.duration_ms) / 1000.0);
    Ok(())
}

fn write_wav(path: &Path, samples: &[f32]) -> anyhow::Result<()> {
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    let data = (samples.len() * 2) as u32;
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data).to_le_bytes())?;
    f.write_all(b"WAVEfmt ")?;
    f.write_all(&16u32.to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&2u16.to_le_bytes())?;
    f.write_all(&RATE.to_le_bytes())?;
    f.write_all(&(RATE * 4).to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?;
    f.write_all(&16u16.to_le_bytes())?;
    f.write_all(b"data")?;
    f.write_all(&data.to_le_bytes())?;
    for s in samples {
        f.write_all(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())?;
    }
    Ok(())
}
