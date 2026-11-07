//! Standard MIDI file export.

use crate::shared::{Note, Quantize};
use midly::num::{u15, u24, u28, u4, u7};
use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
use std::path::{Path, PathBuf};

pub const PPQ: u16 = 480;

/// Converts the notes to ticks, optionally quantizing them to a grid.
fn to_ticks(notes: &[Note], bpm: f64, quantize: Quantize) -> Vec<(u32, u32, u8, u8)> {
    let ticks_per_second = bpm / 60.0 * PPQ as f64;
    let grid = quantize.beats().map(|b| b * PPQ as f64);
    let snap = |t: f64| match grid {
        Some(g) => (t / g).round() * g,
        None => t,
    };

    let mut out: Vec<(u32, u32, u8, u8)> = notes
        .iter()
        .map(|n| {
            let start = snap(n.start * ticks_per_second).max(0.0);
            let mut end = snap(n.end * ticks_per_second);
            let min_len = grid.unwrap_or(PPQ as f64 / 32.0);
            if end - start < min_len {
                end = start + min_len;
            }
            (
                start as u32,
                end as u32,
                n.pitch.min(127),
                n.velocity.clamp(1, 127),
            )
        })
        .collect();

    // Quantizing can make notes of the same pitch overlap, which many DAWs handle badly
    out.sort_by_key(|n| (n.0, n.2));
    for i in 0..out.len() {
        let (start_i, pitch_i) = (out[i].0, out[i].2);
        if let Some(next) = out[i + 1..].iter().find(|n| n.2 == pitch_i).map(|n| n.0) {
            if out[i].1 > next {
                out[i].1 = next.max(start_i + 1);
            }
        }
    }
    out.retain(|n| n.1 > n.0);
    out
}

pub fn encode(notes: &[Note], bpm: f64, quantize: Quantize, track_name: &str) -> Vec<u8> {
    let bpm = if bpm.is_finite() && bpm > 1.0 {
        bpm
    } else {
        120.0
    };
    let notes = to_ticks(notes, bpm, quantize);

    // (absolute tick, is_note_on, pitch, velocity). Note offs sort before note ons on the same
    // tick so repeated notes don't get cut off.
    let mut events: Vec<(u32, bool, u8, u8)> = Vec::with_capacity(notes.len() * 2);
    for &(start, end, pitch, vel) in &notes {
        events.push((start, true, pitch, vel));
        events.push((end, false, pitch, 0));
    }
    events.sort_by_key(|e| (e.0, e.1));

    let micros_per_quarter = (60_000_000.0 / bpm).round() as u32;
    let name_bytes = track_name.as_bytes().to_vec();

    let mut track: Vec<TrackEvent> = vec![
        TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::TrackName(&name_bytes)),
        },
        TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(micros_per_quarter))),
        },
        TrackEvent {
            delta: u28::new(0),
            kind: TrackEventKind::Meta(MetaMessage::TimeSignature(4, 2, 24, 8)),
        },
    ];

    let mut last_tick = 0u32;
    for (tick, on, pitch, vel) in events {
        let delta = tick - last_tick;
        last_tick = tick;
        let message = if on {
            MidiMessage::NoteOn {
                key: u7::new(pitch),
                vel: u7::new(vel),
            }
        } else {
            MidiMessage::NoteOff {
                key: u7::new(pitch),
                vel: u7::new(64),
            }
        };
        track.push(TrackEvent {
            delta: u28::new(delta),
            kind: TrackEventKind::Midi {
                channel: u4::new(0),
                message,
            },
        });
    }
    track.push(TrackEvent {
        delta: u28::new(0),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });

    let smf = Smf {
        header: Header::new(Format::SingleTrack, Timing::Metrical(u15::new(PPQ))),
        tracks: vec![track],
    };
    let mut bytes = Vec::new();
    // Writing into a Vec cannot fail
    let _ = smf.write_std(&mut bytes);
    bytes
}

pub fn save(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("Cannot write {}: {e}", path.display()))
}

/// Writes the MIDI file to a temporary location so it can be dragged into the DAW. DAWs usually
/// name the resulting clip after the file, hence the readable name.
pub fn write_temp(bytes: &[u8], base_name: &str) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("audiotomidi");
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create temp dir: {e}"))?;
    let path = dir.join(format!("{}.mid", sanitize(base_name)));
    save(&path, bytes)?;
    Ok(path)
}

pub fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "audiotomidi".into()
    } else {
        cleaned.chars().take(60).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_parses() {
        let notes = vec![
            Note {
                start: 0.0,
                end: 0.5,
                pitch: 60,
                velocity: 100,
            },
            Note {
                start: 0.5,
                end: 1.0,
                pitch: 64,
                velocity: 90,
            },
        ];
        let bytes = encode(&notes, 120.0, Quantize::Sixteenth, "test");
        let smf = Smf::parse(&bytes).unwrap();
        assert_eq!(smf.tracks.len(), 1);
        let ons: Vec<(u32, u8)> = {
            let mut t = 0u32;
            let mut v = Vec::new();
            for ev in &smf.tracks[0] {
                t += ev.delta.as_int();
                if let TrackEventKind::Midi {
                    message: MidiMessage::NoteOn { key, .. },
                    ..
                } = ev.kind
                {
                    v.push((t, key.as_int()));
                }
            }
            v
        };
        // 0.5 s at 120 BPM = 1 beat = 480 ticks
        assert_eq!(ons, vec![(0, 60), (480, 64)]);
    }
}
