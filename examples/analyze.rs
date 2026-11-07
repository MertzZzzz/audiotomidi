//! Command line front-end for the converter, useful for tuning the analysis:
//! `cargo run --release --example analyze -- input.wav [output.mid]`
use audiotomidi::shared::Settings;
use audiotomidi::{analysis, audio_file, midi_file};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(input) = args.next() else {
        eprintln!("usage: analyze <audio file> [output.mid]");
        std::process::exit(1);
    };
    let decoded = audio_file::load_mono(input.as_ref()).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1);
    });
    let settings = Settings::default();
    let started = std::time::Instant::now();
    let notes = analysis::analyze(&decoded.samples, decoded.sample_rate, &settings);
    eprintln!(
        "{} notes in {:.1} ms",
        notes.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    for n in &notes {
        println!(
            "{:7.3} - {:7.3}  pitch {:3}  vel {:3}",
            n.start, n.end, n.pitch, n.velocity
        );
    }
    if let Some(output) = args.next() {
        let bytes = midi_file::encode(&notes, 120.0, settings.quantize, "audiotomidi");
        midi_file::save(output.as_ref(), &bytes).unwrap();
    }
}
