//! Monophonic audio to MIDI conversion.
//!
//! The pipeline is:
//! 1. low-pass + resample to ~11 kHz (pitch information lives well below that and it makes
//!    everything a lot cheaper),
//! 2. frame-wise fundamental frequency estimation with the YIN algorithm, where the difference
//!    function is computed through FFT based autocorrelation,
//! 3. note segmentation with pitch hysteresis, onset detection and a minimum note length.

use crate::shared::{Note, Settings};
use realfft::num_complex::Complex;
use realfft::RealFftPlanner;

const ANALYSIS_RATE: f32 = 11025.0;
const HOP: usize = 64;
/// Window used for the level gate and onset detection (~23 ms at the analysis rate).
const SHORT_WINDOW: usize = 256;

/// Per-frame analysis result.
#[derive(Debug, Clone, Copy)]
struct Frame {
    /// Fractional MIDI note number, or `None` if the frame is unvoiced/silent.
    midi: Option<f32>,
    db: f32,
}

pub fn analyze(samples: &[f32], sample_rate: f32, settings: &Settings) -> Vec<Note> {
    if samples.is_empty() || sample_rate <= 0.0 {
        return Vec::new();
    }

    let (signal, sr) = downsample(samples, sample_rate);
    let frames = detect_pitch(&signal, sr, settings);
    let frame_time = HOP as f64 / sr as f64;
    let mut notes = segment(&frames, frame_time, settings);

    // The frame time stamps are relative to the window start, shift them to the window center
    let window = window_size(sr, settings) as f64 / sr as f64;
    remove_transition_artifacts(&mut notes, window);
    let duration = samples.len() as f64 / sample_rate as f64;
    for note in &mut notes {
        note.start = (note.start + window * 0.5).min(duration);
        note.end = (note.end + window * 0.5).min(duration);
        note.pitch = (note.pitch as i32 + settings.transpose as i32).clamp(0, 127) as u8;
    }
    notes.retain(|n| n.end > n.start);
    notes
}

fn window_size(sr: f32, settings: &Settings) -> usize {
    let tau_max = max_lag(sr, settings);
    (2 * tau_max).next_power_of_two().clamp(256, 2048)
}

fn max_lag(sr: f32, settings: &Settings) -> usize {
    // Leave some room below the lowest note so it's not right at the edge of the search range
    let f_low = midi_to_hz(settings.low_note as f32 - 1.0);
    ((sr / f_low).ceil() as usize).max(8)
}

fn min_lag(sr: f32, settings: &Settings) -> usize {
    let f_high = midi_to_hz(settings.high_note as f32 + 1.0).min(sr * 0.45);
    ((sr / f_high).floor() as usize).max(2)
}

pub fn midi_to_hz(midi: f32) -> f32 {
    440.0 * 2f32.powf((midi - 69.0) / 12.0)
}

pub fn hz_to_midi(hz: f32) -> f32 {
    69.0 + 12.0 * (hz / 440.0).log2()
}

/// Low-pass filters and resamples the signal to roughly [`ANALYSIS_RATE`].
fn downsample(samples: &[f32], sample_rate: f32) -> (Vec<f32>, f32) {
    if sample_rate <= ANALYSIS_RATE * 1.25 {
        return (samples.to_vec(), sample_rate);
    }

    let cutoff = ANALYSIS_RATE * 0.42;
    let mut stages = [
        Biquad::lowpass(cutoff, sample_rate, 0.541),
        Biquad::lowpass(cutoff, sample_rate, 1.307),
    ];
    let filtered: Vec<f32> = samples
        .iter()
        .map(|&s| stages.iter_mut().fold(s, |acc, st| st.process(acc)))
        .collect();

    let ratio = sample_rate as f64 / ANALYSIS_RATE as f64;
    let out_len = (filtered.len() as f64 / ratio).floor() as usize;
    let mut out = Vec::with_capacity(out_len);
    for i in 0..out_len {
        let pos = i as f64 * ratio;
        let idx = pos as usize;
        let frac = (pos - idx as f64) as f32;
        let a = filtered[idx];
        let b = *filtered.get(idx + 1).unwrap_or(&a);
        out.push(a + (b - a) * frac);
    }
    let actual_rate = (sample_rate as f64 / ratio) as f32;
    (out, actual_rate)
}

struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Biquad {
    fn lowpass(freq: f32, sr: f32, q: f32) -> Self {
        let w0 = 2.0 * std::f32::consts::PI * freq / sr;
        let alpha = w0.sin() / (2.0 * q);
        let cos = w0.cos();
        let a0 = 1.0 + alpha;
        Biquad {
            b0: (1.0 - cos) / 2.0 / a0,
            b1: (1.0 - cos) / a0,
            b2: (1.0 - cos) / 2.0 / a0,
            a1: -2.0 * cos / a0,
            a2: (1.0 - alpha) / a0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

fn detect_pitch(signal: &[f32], sr: f32, settings: &Settings) -> Vec<Frame> {
    let w = window_size(sr, settings);
    let tau_max = max_lag(sr, settings).min(w - 1);
    let tau_min = min_lag(sr, settings).min(tau_max - 1);
    let n_fft = (w + tau_max).next_power_of_two();

    let mut planner = RealFftPlanner::<f32>::new();
    let fwd = planner.plan_fft_forward(n_fft);
    let inv = planner.plan_fft_inverse(n_fft);

    let mut a_buf = fwd.make_input_vec();
    let mut b_buf = fwd.make_input_vec();
    let mut a_spec = fwd.make_output_vec();
    let mut b_spec = fwd.make_output_vec();
    let mut corr = inv.make_output_vec();
    let mut scratch_fwd = fwd.make_scratch_vec();
    let mut scratch_inv = inv.make_scratch_vec();

    // Prefix sums of the squared signal for the energy terms of the difference function
    let mut energy = Vec::with_capacity(signal.len() + 1);
    energy.push(0.0f64);
    for &s in signal {
        let last = *energy.last().unwrap();
        energy.push(last + (s as f64) * (s as f64));
    }
    let window_energy = |from: usize, len: usize| -> f64 {
        let to = (from + len).min(signal.len());
        let from = from.min(to);
        energy[to] - energy[from]
    };

    let gate = db_to_gain(settings.gate_db);
    let threshold = settings.sensitivity.clamp(0.02, 0.9);

    let mut diff = vec![0.0f32; tau_max + 1];
    let mut cmnd = vec![1.0f32; tau_max + 1];

    let mut frames = Vec::new();
    let mut t = 0;
    while t + w <= signal.len() {
        let e0 = window_energy(t, w);
        // Level detection uses a short window around the frame's center. The pitch window is
        // long, and would otherwise smear over short rests between notes.
        let short_start = (t + w / 2).saturating_sub(SHORT_WINDOW / 2);
        let rms = (window_energy(short_start, SHORT_WINDOW) / SHORT_WINDOW as f64).sqrt() as f32;
        let db = gain_to_db(rms);

        if rms < gate {
            frames.push(Frame { midi: None, db });
            t += HOP;
            continue;
        }

        // r(tau) = sum_j a[j] * b[j + tau] through FFT cross correlation
        a_buf.iter_mut().for_each(|x| *x = 0.0);
        b_buf.iter_mut().for_each(|x| *x = 0.0);
        a_buf[..w].copy_from_slice(&signal[t..t + w]);
        let b_len = (w + tau_max).min(signal.len() - t);
        b_buf[..b_len].copy_from_slice(&signal[t..t + b_len]);

        let ok = fwd
            .process_with_scratch(&mut a_buf, &mut a_spec, &mut scratch_fwd)
            .is_ok()
            && fwd
                .process_with_scratch(&mut b_buf, &mut b_spec, &mut scratch_fwd)
                .is_ok();
        if !ok {
            frames.push(Frame { midi: None, db });
            t += HOP;
            continue;
        }
        for (a, b) in a_spec.iter_mut().zip(b_spec.iter()) {
            *a = a.conj() * b;
        }
        // The DC and Nyquist bins of a real signal's spectrum must be purely real
        if let Some(first) = a_spec.first_mut() {
            *first = Complex::new(first.re, 0.0);
        }
        if let Some(last) = a_spec.last_mut() {
            *last = Complex::new(last.re, 0.0);
        }
        if inv
            .process_with_scratch(&mut a_spec, &mut corr, &mut scratch_inv)
            .is_err()
        {
            frames.push(Frame { midi: None, db });
            t += HOP;
            continue;
        }
        let norm = 1.0 / n_fft as f32;

        diff[0] = 0.0;
        for tau in 1..=tau_max {
            let e_tau = window_energy(t + tau, w) as f32;
            diff[tau] = (e0 as f32 + e_tau - 2.0 * corr[tau] * norm).max(0.0);
        }

        // Cumulative mean normalized difference
        cmnd[0] = 1.0;
        let mut running = 0.0f32;
        for tau in 1..=tau_max {
            running += diff[tau];
            cmnd[tau] = if running > 0.0 {
                diff[tau] * tau as f32 / running
            } else {
                1.0
            };
        }

        // Absolute threshold: the first dip below the threshold, then walk down to its minimum
        let mut best = None;
        let mut tau = tau_min;
        while tau <= tau_max {
            if cmnd[tau] < threshold {
                while tau < tau_max && cmnd[tau + 1] < cmnd[tau] {
                    tau += 1;
                }
                best = Some(tau);
                break;
            }
            tau += 1;
        }

        let midi = best.and_then(|tau| {
            // Parabolic interpolation for sub-sample accuracy
            let refined = if tau > 1 && tau < tau_max {
                let (s0, s1, s2) = (cmnd[tau - 1], cmnd[tau], cmnd[tau + 1]);
                let denom = 2.0 * (2.0 * s1 - s2 - s0);
                if denom.abs() > 1e-9 {
                    tau as f32 + (s2 - s0) / denom
                } else {
                    tau as f32
                }
            } else {
                tau as f32
            };
            let hz = sr / refined;
            let midi = hz_to_midi(hz);
            let lo = settings.low_note as f32 - 0.5;
            let hi = settings.high_note as f32 + 0.5;
            (midi.is_finite() && midi >= lo && midi <= hi).then_some(midi)
        });

        frames.push(Frame { midi, db });
        t += HOP;
    }

    median_smooth(&mut frames);
    frames
}

/// 5 point median filter over the pitch track to get rid of octave jumps and other outliers.
fn median_smooth(frames: &mut [Frame]) {
    let original: Vec<Option<f32>> = frames.iter().map(|f| f.midi).collect();
    for i in 0..frames.len() {
        if original[i].is_none() {
            continue;
        }
        let lo = i.saturating_sub(2);
        let hi = (i + 3).min(original.len());
        let mut values: Vec<f32> = original[lo..hi].iter().flatten().copied().collect();
        if values.len() >= 3 {
            values.sort_by(|a, b| a.total_cmp(b));
            frames[i].midi = Some(values[values.len() / 2]);
        }
    }
}

struct Pending {
    start_frame: usize,
    last_voiced: usize,
    pitches: Vec<f32>,
    peak_db: f32,
}

impl Pending {
    fn pitch(&self) -> u8 {
        let mut p = self.pitches.clone();
        p.sort_by(|a, b| a.total_cmp(b));
        p[p.len() / 2].round().clamp(0.0, 127.0) as u8
    }
}

fn segment(frames: &[Frame], frame_time: f64, settings: &Settings) -> Vec<Note> {
    // Number of consecutive frames needed to accept a pitch change or end a note
    const CHANGE_FRAMES: usize = 4;
    const RELEASE_FRAMES: usize = 3;
    const ONSET_DB: f32 = 9.0;

    let min_frames = ((settings.min_note_ms as f64 / 1000.0) / frame_time).ceil() as usize;
    let mut notes = Vec::new();
    let mut current: Option<Pending> = None;
    let mut candidate: Option<(u8, usize)> = None;

    let finish = |p: Pending, end_frame: usize, notes: &mut Vec<Note>| {
        let len = end_frame.saturating_sub(p.start_frame);
        if len >= min_frames.max(1) && !p.pitches.is_empty() {
            notes.push(Note {
                start: p.start_frame as f64 * frame_time,
                end: end_frame as f64 * frame_time,
                pitch: p.pitch(),
                velocity: db_to_velocity(p.peak_db),
            });
        }
    };

    for (i, frame) in frames.iter().enumerate() {
        let onset = i >= 3 && frame.db - frames[i - 3].db > ONSET_DB;

        match frame.midi {
            Some(midi) => {
                let rounded = midi.round().clamp(0.0, 127.0) as u8;
                match current.as_mut() {
                    None => {
                        current = Some(Pending {
                            start_frame: i,
                            last_voiced: i,
                            pitches: vec![midi],
                            peak_db: frame.db,
                        });
                        candidate = None;
                    }
                    Some(note) => {
                        let note_pitch = note.pitch();
                        // Re-attack on the same pitch after a clear level jump
                        let reattack = onset && i - note.start_frame > min_frames;
                        let differs = (midi - note_pitch as f32).abs() > 0.65;

                        if reattack {
                            let old = current.take().unwrap();
                            let end = old.last_voiced + 1;
                            finish(old, end, &mut notes);
                            current = Some(Pending {
                                start_frame: i,
                                last_voiced: i,
                                pitches: vec![midi],
                                peak_db: frame.db,
                            });
                            candidate = None;
                        } else if differs {
                            let count = match candidate {
                                Some((p, c)) if p == rounded => c + 1,
                                _ => 1,
                            };
                            if count >= CHANGE_FRAMES {
                                // The new pitch started `count` frames ago
                                let split = i + 1 - count;
                                let old = current.take().unwrap();
                                finish(old, split, &mut notes);
                                let start_pitches =
                                    frames[split..=i].iter().filter_map(|f| f.midi).collect();
                                current = Some(Pending {
                                    start_frame: split,
                                    last_voiced: i,
                                    pitches: start_pitches,
                                    peak_db: frame.db,
                                });
                                candidate = None;
                            } else {
                                candidate = Some((rounded, count));
                                note.last_voiced = i;
                            }
                        } else {
                            candidate = None;
                            note.pitches.push(midi);
                            note.last_voiced = i;
                            note.peak_db = note.peak_db.max(frame.db);
                        }
                    }
                }
            }
            None => {
                if let Some(note) = current.as_ref() {
                    if i - note.last_voiced >= RELEASE_FRAMES {
                        let old = current.take().unwrap();
                        let end = old.last_voiced + 1;
                        finish(old, end, &mut notes);
                        candidate = None;
                    }
                }
            }
        }
    }
    if let Some(note) = current.take() {
        let end = note.last_voiced + 1;
        finish(note, end, &mut notes);
    }

    // Glue together same-pitched fragments separated by tiny gaps
    let mut merged: Vec<Note> = Vec::with_capacity(notes.len());
    for note in notes {
        if let Some(prev) = merged.last_mut() {
            if prev.pitch == note.pitch && note.start - prev.end < 0.025 {
                prev.end = note.end;
                prev.velocity = prev.velocity.max(note.velocity);
                continue;
            }
        }
        merged.push(note);
    }
    merged
}

/// During a legato transition the analysis window contains both pitches, and YIN tends to lock
/// onto their common period (usually far below both). That shows up as a very short note glued
/// in between two others with a large interval to both; such notes are given to the next note.
fn remove_transition_artifacts(notes: &mut Vec<Note>, window: f64) {
    const TOUCH: f64 = 0.012;
    let mut i = 1;
    while i + 1 < notes.len() {
        let (prev, cur, next) = (notes[i - 1], notes[i], notes[i + 1]);
        let touching = cur.start - prev.end < TOUCH && next.start - cur.end < TOUCH;
        let short = cur.end - cur.start < window;
        let jump = |a: u8, b: u8| (a as i32 - b as i32).abs() >= 5;
        if touching && short && jump(cur.pitch, prev.pitch) && jump(cur.pitch, next.pitch) {
            notes[i + 1].start = cur.start;
            notes.remove(i);
        } else {
            i += 1;
        }
    }
}

fn db_to_velocity(db: f32) -> u8 {
    let t = ((db + 50.0) / 50.0).clamp(0.0, 1.0);
    (20.0 + t * 107.0).round() as u8
}

fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-9).log10()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(hz: f32, seconds: f32, sr: f32, amp: f32) -> Vec<f32> {
        let n = (seconds * sr) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / sr;
                // A few harmonics to make it a bit more realistic than a pure sine
                amp * ((2.0 * std::f32::consts::PI * hz * t).sin()
                    + 0.5 * (4.0 * std::f32::consts::PI * hz * t).sin()
                    + 0.25 * (6.0 * std::f32::consts::PI * hz * t).sin())
                    / 1.75
            })
            .collect()
    }

    #[test]
    fn detects_simple_melody() {
        let sr = 44100.0;
        let mut signal = Vec::new();
        signal.extend(tone(440.0, 0.5, sr, 0.5)); // A4
        signal.extend(vec![0.0; (0.2 * sr) as usize]);
        signal.extend(tone(220.0, 0.5, sr, 0.5)); // A3
        signal.extend(tone(329.63, 0.5, sr, 0.5)); // E4, legato
        signal.extend(vec![0.0; (0.2 * sr) as usize]);
        signal.extend(tone(82.41, 0.6, sr, 0.5)); // E2

        let notes = analyze(&signal, sr, &Settings::default());
        let pitches: Vec<u8> = notes.iter().map(|n| n.pitch).collect();
        assert_eq!(pitches, vec![69, 57, 64, 40], "{notes:#?}");

        // Rough timing checks
        assert!((notes[0].start - 0.0).abs() < 0.06, "{notes:#?}");
        assert!((notes[0].end - 0.5).abs() < 0.08, "{notes:#?}");
        assert!((notes[1].start - 0.7).abs() < 0.06, "{notes:#?}");
        assert!((notes[2].start - 1.2).abs() < 0.08, "{notes:#?}");
    }

    #[test]
    fn short_rest_splits_repeated_notes() {
        let sr = 44100.0;
        let mut signal = tone(440.0, 0.4, sr, 0.5);
        signal.extend(vec![0.0; (0.1 * sr) as usize]);
        signal.extend(tone(440.0, 0.4, sr, 0.5));
        let notes = analyze(&signal, sr, &Settings::default());
        assert_eq!(notes.len(), 2, "{notes:#?}");
        assert!(notes.iter().all(|n| n.pitch == 69));
        assert!((notes[1].start - 0.5).abs() < 0.05, "{notes:#?}");
    }

    #[test]
    fn silence_has_no_notes() {
        let notes = analyze(&vec![0.0; 44100], 44100.0, &Settings::default());
        assert!(notes.is_empty());
    }

    #[test]
    fn transpose_is_applied() {
        let sr = 48000.0;
        let signal = tone(261.63, 0.5, sr, 0.5); // C4
        let settings = Settings {
            transpose: 12,
            ..Settings::default()
        };
        let notes = analyze(&signal, sr, &settings);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].pitch, 72);
    }
}
