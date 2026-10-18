//! Prints what analysis finds in audio files: `cargo run --example analyze -- FILE...`

use dejay_audio::{analysis, pcm};
use std::path::Path;

fn main() {
    for file in std::env::args().skip(1) {
        let decoded = match pcm::decode(Path::new("ffmpeg"), Path::new(&file), 48_000) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{file}: {e:#}");
                continue;
            }
        };
        let started = std::time::Instant::now();
        let out = analysis::analyze(&decoded);
        let a = out.analysis;
        println!("{file}");
        println!("  analysed {:.1}s of audio in {:.2}s", decoded.duration_ms() / 1000.0, started.elapsed().as_secs_f64());
        println!("  grid {:?}", a.grid);
        println!("  key {:?}  energy {:.2}  loudness {:.1} LUFS  music {:.0}..{:.0} ms", a.key.map(|k| k.name()), a.energy, a.loudness, a.start_ms, a.end_ms);
        if let Some(g) = a.grid {
            let [(ks, kp), (fs, fp)] = analysis::phase_report(&decoded, g.bpm);
            println!("  kick strength {ks:.2} first beat {kp:.1} ms; all onsets {fs:.2} at {fp:.1} ms; beat {:.1} ms", g.beat_ms());
        }
        for (bpm, score) in analysis::tempo_candidates(&decoded).into_iter().take(6) {
            println!("  candidate {bpm:7.2} BPM  score {score:.4}");
        }
    }
}
