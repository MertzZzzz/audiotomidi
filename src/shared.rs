//! State shared between the audio thread, the GUI and the background workers.

use atomic_float::{AtomicF32, AtomicF64};
use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Number of samples summarized by one waveform overview bucket.
pub const PEAK_BLOCK: usize = 256;

/// Upper bound for the recording length (in seconds) to avoid eating all memory by accident.
pub const MAX_RECORD_SECONDS: f32 = 15.0 * 60.0;

/// A detected note. Times are in seconds relative to the start of the clip.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub start: f64,
    pub end: f64,
    pub pitch: u8,
    /// MIDI velocity, 1..=127.
    pub velocity: u8,
}

/// The audio material that is being converted, always mono.
#[derive(Default)]
pub struct Clip {
    pub name: String,
    pub sample_rate: f32,
    pub samples: Vec<f32>,
    /// `(min, max)` for every [`PEAK_BLOCK`] samples, used to draw the waveform cheaply.
    pub peaks: Vec<(f32, f32)>,
}

impl Clip {
    pub fn new(name: String, sample_rate: f32, samples: Vec<f32>) -> Self {
        let mut clip = Clip {
            name,
            sample_rate,
            samples: Vec::new(),
            peaks: Vec::new(),
        };
        clip.append(&samples);
        clip
    }

    pub fn duration(&self) -> f64 {
        if self.sample_rate > 0.0 {
            self.samples.len() as f64 / self.sample_rate as f64
        } else {
            0.0
        }
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Appends samples and keeps the peak overview up to date.
    pub fn append(&mut self, new: &[f32]) {
        let start = self.samples.len();
        self.samples.extend_from_slice(new);

        // The last bucket may have been partial, so recompute from its start
        let first_block = start / PEAK_BLOCK;
        self.peaks.truncate(first_block);
        for block in self.samples[first_block * PEAK_BLOCK..].chunks(PEAK_BLOCK) {
            let (mut lo, mut hi) = (0.0f32, 0.0f32);
            for &s in block {
                lo = lo.min(s);
                hi = hi.max(s);
            }
            self.peaks.push((lo, hi));
        }
    }
}

/// User-tweakable analysis/export settings (GUI only, not automatable).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    /// YIN threshold. Higher = more permissive pitch detection (more notes, more garbage).
    pub sensitivity: f32,
    /// Frames quieter than this (dBFS) are treated as silence.
    pub gate_db: f32,
    /// Notes shorter than this are discarded.
    pub min_note_ms: f32,
    /// Lowest and highest MIDI note that may be detected.
    pub low_note: u8,
    pub high_note: u8,
    /// Semitone offset applied to the result.
    pub transpose: i8,
    /// Quantization grid for the exported MIDI file.
    pub quantize: Quantize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sensitivity: 0.2,
            gate_db: -45.0,
            min_note_ms: 60.0,
            low_note: 28,  // E1
            high_note: 96, // C7
            transpose: 0,
            quantize: Quantize::Off,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quantize {
    Off,
    Quarter,
    Eighth,
    Sixteenth,
    ThirtySecond,
}

impl Quantize {
    pub const ALL: [Quantize; 5] = [
        Quantize::Off,
        Quantize::Quarter,
        Quantize::Eighth,
        Quantize::Sixteenth,
        Quantize::ThirtySecond,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quantize::Off => "Off",
            Quantize::Quarter => "1/4",
            Quantize::Eighth => "1/8",
            Quantize::Sixteenth => "1/16",
            Quantize::ThirtySecond => "1/32",
        }
    }

    /// Grid size in quarter notes.
    pub fn beats(self) -> Option<f64> {
        match self {
            Quantize::Off => None,
            Quantize::Quarter => Some(1.0),
            Quantize::Eighth => Some(0.5),
            Quantize::Sixteenth => Some(0.25),
            Quantize::ThirtySecond => Some(0.125),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    Idle = 0,
    Recording = 1,
    Loading = 2,
    Analyzing = 3,
    Done = 4,
    Error = 5,
}

impl Status {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Status::Recording,
            2 => Status::Loading,
            3 => Status::Analyzing,
            4 => Status::Done,
            5 => Status::Error,
            _ => Status::Idle,
        }
    }
}

pub struct Shared {
    pub clip: RwLock<Clip>,
    pub notes: Arc<RwLock<Vec<Note>>>,
    pub settings: Arc<RwLock<Settings>>,

    status: AtomicU8,
    pub message: Mutex<String>,
    /// Incremented whenever `notes` changes so the GUI can invalidate caches.
    pub notes_generation: AtomicU64,
    /// Incremented whenever the clip is replaced, so stale analysis results can be discarded.
    pub clip_generation: AtomicU64,
    pub analyzing: AtomicBool,
    /// Host transport position (seconds) at which the current recording started. Used to place
    /// the notes at the right time when playing them back through the MIDI output.
    pub clip_offset: AtomicF64,
    /// Set when recording starts, the audio thread then stores the transport position.
    pub need_offset: AtomicBool,

    /// Set by the GUI, read by the audio thread.
    pub recording: AtomicBool,
    /// Whether the audio thread should play the detected notes on the MIDI output in sync with the
    /// host transport.
    pub midi_out_enabled: AtomicBool,

    pub host_tempo: AtomicF64,
    pub host_playing: AtomicBool,
    pub sample_rate: AtomicF32,
    pub input_level: AtomicF32,

    /// Samples captured by the audio thread go through here, a worker drains it into `clip`.
    record_consumer: Mutex<Option<rtrb::Consumer<f32>>>,
    shutdown: AtomicBool,
    drain_thread: Mutex<Option<JoinHandle<()>>>,
}

impl Shared {
    pub fn new(notes: Arc<RwLock<Vec<Note>>>, settings: Arc<RwLock<Settings>>) -> Arc<Self> {
        let shared = Arc::new(Shared {
            clip: RwLock::new(Clip::default()),
            notes,
            settings,
            status: AtomicU8::new(Status::Idle as u8),
            message: Mutex::new(String::new()),
            notes_generation: AtomicU64::new(0),
            clip_generation: AtomicU64::new(0),
            analyzing: AtomicBool::new(false),
            clip_offset: AtomicF64::new(0.0),
            need_offset: AtomicBool::new(false),
            recording: AtomicBool::new(false),
            midi_out_enabled: AtomicBool::new(false),
            host_tempo: AtomicF64::new(120.0),
            host_playing: AtomicBool::new(false),
            sample_rate: AtomicF32::new(44100.0),
            input_level: AtomicF32::new(0.0),
            record_consumer: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            drain_thread: Mutex::new(None),
        });

        // A small worker that moves recorded samples from the lock-free ring buffer into the clip.
        // Doing this outside of the GUI means recording also works with the editor closed.
        let weak = Arc::downgrade(&shared);
        let handle = std::thread::Builder::new()
            .name("audiotomidi-recorder".into())
            .spawn(move || {
                let mut scratch = Vec::with_capacity(1 << 16);
                loop {
                    std::thread::sleep(Duration::from_millis(15));
                    let Some(shared) = weak.upgrade() else { break };
                    if shared.shutdown.load(Ordering::Relaxed) {
                        break;
                    }
                    shared.drain_recording(&mut scratch);
                }
            })
            .ok();
        *shared.drain_thread.lock() = handle;

        shared
    }

    pub fn status(&self) -> Status {
        Status::from_u8(self.status.load(Ordering::Acquire))
    }

    pub fn set_status(&self, status: Status, message: impl Into<String>) {
        self.status.store(status as u8, Ordering::Release);
        *self.message.lock() = message.into();
    }

    pub fn set_notes(&self, notes: Vec<Note>) {
        *self.notes.write() = notes;
        self.notes_generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Installs a fresh ring buffer for the audio thread. Returns the producer side, which is
    /// handed to the audio thread through `Plugin::initialize()`.
    pub fn new_record_channel(&self, sample_rate: f32) -> rtrb::Producer<f32> {
        // Four seconds of headroom is plenty, the drain thread runs every 15 ms
        let capacity = (sample_rate as usize * 4).max(1 << 16);
        let (producer, consumer) = rtrb::RingBuffer::new(capacity);
        *self.record_consumer.lock() = Some(consumer);
        producer
    }

    fn drain_recording(&self, scratch: &mut Vec<f32>) {
        scratch.clear();
        if let Some(consumer) = self.record_consumer.lock().as_mut() {
            let available = consumer.slots();
            if available == 0 {
                return;
            }
            if let Ok(chunk) = consumer.read_chunk(available) {
                let (a, b) = chunk.as_slices();
                scratch.extend_from_slice(a);
                scratch.extend_from_slice(b);
                chunk.commit_all();
            }
        }
        if scratch.is_empty() || !self.recording.load(Ordering::Acquire) {
            return;
        }

        let mut clip = self.clip.write();
        let max_len = (MAX_RECORD_SECONDS * clip.sample_rate.max(1.0)) as usize;
        if clip.samples.len() + scratch.len() > max_len {
            let room = max_len.saturating_sub(clip.samples.len());
            clip.append(&scratch[..room]);
            drop(clip);
            self.recording.store(false, Ordering::Release);
            self.set_status(Status::Idle, "Recording stopped: maximum length reached");
        } else {
            clip.append(scratch);
        }
    }

    /// Replaces the current clip and clears the old result.
    pub fn replace_clip(&self, clip: Clip) {
        *self.clip.write() = clip;
        self.clip_generation.fetch_add(1, Ordering::AcqRel);
        self.set_notes(Vec::new());
    }

    /// Starts recording into a new, empty clip.
    pub fn start_recording(&self) {
        let sr = self.sample_rate.load(Ordering::Relaxed);
        // Throw away whatever was sitting in the ring buffer from before
        if let Some(consumer) = self.record_consumer.lock().as_mut() {
            let n = consumer.slots();
            if let Ok(chunk) = consumer.read_chunk(n) {
                chunk.commit_all();
            }
        }
        self.replace_clip(Clip::new("Recording".into(), sr, Vec::new()));
        self.clip_offset.store(0.0, Ordering::Relaxed);
        self.need_offset.store(true, Ordering::Release);
        self.recording.store(true, Ordering::Release);
        self.set_status(Status::Recording, "Recording input...");
    }

    pub fn stop_recording(&self) {
        // Give the drain thread a chance to pick up the last few samples
        let mut scratch = Vec::new();
        self.drain_recording(&mut scratch);
        self.recording.store(false, Ordering::Release);
        self.set_status(Status::Idle, "");
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        // The drain thread only holds a weak reference, it will exit by itself. We must not join
        // it here since this may run on that very thread.
        let _ = self.drain_thread.lock().take();
    }
}
