//! Audio To MIDI: a VST3/CLAP plugin that records or loads audio, converts the (monophonic)
//! melody to MIDI notes and lets you drag the result straight into the DAW.

use nih_plug::prelude::*;
use nih_plug_egui::EguiState;
use parking_lot::RwLock;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub mod analysis;
pub mod audio_file;
mod drag_out;
mod editor;
pub mod midi_file;
pub mod shared;

use shared::{Note, Settings, Shared};

pub struct AudioToMidi {
    params: Arc<AudioToMidiParams>,
    shared: Arc<Shared>,

    sample_rate: f32,
    record_producer: Option<rtrb::Producer<f32>>,
    level: f32,
    level_decay: f32,

    /// Notes currently held on the MIDI output, per key.
    active_notes: [bool; 128],
    /// The transport position we expect at the start of the next block, to detect jumps.
    expected_pos: Option<f64>,
    events: Vec<(u32, bool, u8, f32)>,
}

#[derive(Params)]
pub struct AudioToMidiParams {
    #[persist = "editor-state"]
    editor_state: Arc<EguiState>,
    /// The conversion result is stored with the project.
    #[persist = "notes"]
    notes: Arc<RwLock<Vec<Note>>>,
    #[persist = "settings"]
    settings: Arc<RwLock<Settings>>,

    /// Whether the input audio is passed through to the output.
    #[id = "thru"]
    pub thru: BoolParam,
}

impl Default for AudioToMidiParams {
    fn default() -> Self {
        Self {
            editor_state: EguiState::from_size(editor::WIDTH, editor::HEIGHT),
            notes: Arc::new(RwLock::new(Vec::new())),
            settings: Arc::new(RwLock::new(Settings::default())),
            thru: BoolParam::new("Audio Thru", true),
        }
    }
}

impl Default for AudioToMidi {
    fn default() -> Self {
        let params = Arc::new(AudioToMidiParams::default());
        let shared = Shared::new(params.notes.clone(), params.settings.clone());
        Self {
            params,
            shared,
            sample_rate: 44100.0,
            record_producer: None,
            level: 0.0,
            level_decay: 0.999,
            active_notes: [false; 128],
            expected_pos: None,
            events: Vec::with_capacity(512),
        }
    }
}

impl AudioToMidi {
    fn release_all(&mut self, context: &mut impl ProcessContext<Self>, timing: u32) {
        for (note, active) in self.active_notes.iter_mut().enumerate() {
            if *active {
                *active = false;
                context.send_event(NoteEvent::NoteOff {
                    timing,
                    voice_id: None,
                    channel: 0,
                    note: note as u8,
                    velocity: 0.0,
                });
            }
        }
    }

    /// Plays the detected notes on the MIDI output, following the host's transport.
    fn play_midi(&mut self, num_samples: usize, context: &mut impl ProcessContext<Self>) {
        let transport = context.transport();
        let playing = transport.playing;
        let pos = transport.pos_seconds();

        let enabled = self.shared.midi_out_enabled.load(Ordering::Relaxed);
        let (Some(t0), true, true) = (pos, playing, enabled) else {
            self.expected_pos = None;
            self.release_all(context, 0);
            return;
        };

        let block = num_samples as f64 / self.sample_rate as f64;
        if let Some(expected) = self.expected_pos {
            if (t0 - expected).abs() > block.max(0.002) {
                // The user jumped around on the timeline
                self.release_all(context, 0);
            }
        }
        self.expected_pos = Some(t0 + block);
        let t1 = t0 + block;

        let offset = self.shared.clip_offset.load(Ordering::Relaxed);
        let Some(notes) = self.shared.notes.try_read() else {
            return;
        };

        self.events.clear();
        let to_timing = |t: f64| -> u32 {
            (((t - t0) * self.sample_rate as f64) as i64).clamp(0, num_samples as i64 - 1) as u32
        };
        for note in notes.iter() {
            let start = note.start + offset;
            let end = note.end + offset;
            if start >= t1 {
                // Notes are sorted by their start time
                break;
            }
            if end >= t0 && end < t1 && self.active_notes[note.pitch as usize] {
                self.events.push((to_timing(end), false, note.pitch, 0.0));
            }
            if start >= t0 && start < t1 && self.events.len() < self.events.capacity() - 1 {
                self.events.push((
                    to_timing(start),
                    true,
                    note.pitch,
                    note.velocity as f32 / 127.0,
                ));
            }
        }
        drop(notes);

        // Offs before ons at the same sample
        self.events
            .sort_unstable_by_key(|&(timing, on, _, _)| (timing, on));
        for i in 0..self.events.len() {
            let (timing, on, note, velocity) = self.events[i];
            if on {
                if self.active_notes[note as usize] {
                    context.send_event(NoteEvent::NoteOff {
                        timing,
                        voice_id: None,
                        channel: 0,
                        note,
                        velocity: 0.0,
                    });
                }
                self.active_notes[note as usize] = true;
                context.send_event(NoteEvent::NoteOn {
                    timing,
                    voice_id: None,
                    channel: 0,
                    note,
                    velocity,
                });
            } else if self.active_notes[note as usize] {
                self.active_notes[note as usize] = false;
                context.send_event(NoteEvent::NoteOff {
                    timing,
                    voice_id: None,
                    channel: 0,
                    note,
                    velocity: 0.0,
                });
            }
        }
    }
}

impl Plugin for AudioToMidi {
    const NAME: &'static str = "Audio To MIDI";
    const VENDOR: &'static str = "audiotomidi";
    const URL: &'static str = "https://github.com/MertzZzzz/audiotomidi";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] = &[
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(2),
            main_output_channels: NonZeroU32::new(2),
            ..AudioIOLayout::const_default()
        },
        AudioIOLayout {
            main_input_channels: NonZeroU32::new(1),
            main_output_channels: NonZeroU32::new(1),
            ..AudioIOLayout::const_default()
        },
    ];

    const MIDI_INPUT: MidiConfig = MidiConfig::None;
    const MIDI_OUTPUT: MidiConfig = MidiConfig::Basic;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = ();

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn editor(&mut self, _async_executor: AsyncExecutor<Self>) -> Option<Box<dyn Editor>> {
        editor::create(self.params.editor_state.clone(), self.shared.clone())
    }

    fn initialize(
        &mut self,
        _audio_io_layout: &AudioIOLayout,
        buffer_config: &BufferConfig,
        _context: &mut impl InitContext<Self>,
    ) -> bool {
        self.sample_rate = buffer_config.sample_rate;
        self.shared
            .sample_rate
            .store(buffer_config.sample_rate, Ordering::Relaxed);
        self.record_producer = Some(self.shared.new_record_channel(buffer_config.sample_rate));
        // Level meter falls by ~20 dB in 300 ms
        self.level_decay = 0.1f32.powf(1.0 / (0.3 * buffer_config.sample_rate));
        true
    }

    fn reset(&mut self) {
        self.active_notes = [false; 128];
        self.expected_pos = None;
        self.level = 0.0;
    }

    fn process(
        &mut self,
        buffer: &mut Buffer,
        _aux: &mut AuxiliaryBuffers,
        context: &mut impl ProcessContext<Self>,
    ) -> ProcessStatus {
        let transport = context.transport();
        if let Some(tempo) = transport.tempo {
            self.shared.host_tempo.store(tempo, Ordering::Relaxed);
        }
        self.shared
            .host_playing
            .store(transport.playing, Ordering::Relaxed);

        let recording = self.shared.recording.load(Ordering::Acquire);
        if recording && self.shared.need_offset.swap(false, Ordering::AcqRel) {
            let offset = if transport.playing {
                transport.pos_seconds().unwrap_or(0.0)
            } else {
                0.0
            };
            self.shared.clip_offset.store(offset, Ordering::Relaxed);
        }

        let num_samples = buffer.samples();
        let thru = self.params.thru.value();
        for mut frame in buffer.iter_samples() {
            let channels = frame.len().max(1);
            let mut mono = 0.0;
            for sample in frame.iter_mut() {
                mono += *sample;
                if !thru {
                    *sample = 0.0;
                }
            }
            mono /= channels as f32;

            self.level = (self.level * self.level_decay).max(mono.abs());
            if recording {
                if let Some(producer) = self.record_producer.as_mut() {
                    // If the ring buffer is full we drop samples rather than block
                    let _ = producer.push(mono);
                }
            }
        }
        self.shared.input_level.store(self.level, Ordering::Relaxed);

        self.play_midi(num_samples, context);

        ProcessStatus::Normal
    }
}

impl ClapPlugin for AudioToMidi {
    const CLAP_ID: &'static str = "com.github.mertzzzzz.audiotomidi";
    const CLAP_DESCRIPTION: Option<&'static str> =
        Some("Converts audio to MIDI and lets you drag it into the piano roll");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[
        ClapFeature::AudioEffect,
        ClapFeature::Analyzer,
        ClapFeature::NoteEffect,
        ClapFeature::Stereo,
        ClapFeature::Mono,
        ClapFeature::Utility,
    ];
}

impl Vst3Plugin for AudioToMidi {
    const VST3_CLASS_ID: [u8; 16] = *b"AudioToMidiMertz";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[
        Vst3SubCategory::Fx,
        Vst3SubCategory::Analyzer,
        Vst3SubCategory::Tools,
    ];
}

nih_export_clap!(AudioToMidi);
nih_export_vst3!(AudioToMidi);
