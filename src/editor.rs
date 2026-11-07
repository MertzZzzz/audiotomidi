//! The egui based editor: an input panel (record / open audio) and an output panel (piano roll
//! preview, drag & drop into the DAW, save as .mid) that share a single time axis.

use crate::drag_out::{self, RawParent};
use crate::shared::{Clip, Quantize, Settings, Shared, Status};
use crate::{analysis, audio_file, midi_file};
use nih_plug::prelude::{Editor, GuiContext, ParentWindowHandle};
use nih_plug_egui::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Shape, Stroke,
    StrokeKind, Ui, UiBuilder, Vec2,
};
use nih_plug_egui::{create_egui_editor, EguiState};
use parking_lot::Mutex;
use std::any::Any;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const WIDTH: u32 = 960;
pub const HEIGHT: u32 = 580;

mod palette {
    use super::Color32;
    pub const BG: Color32 = Color32::from_rgb(0x10, 0x11, 0x17);
    pub const CARD: Color32 = Color32::from_rgb(0x19, 0x1b, 0x24);
    pub const SURFACE: Color32 = Color32::from_rgb(0x12, 0x13, 0x1a);
    pub const BORDER: Color32 = Color32::from_rgb(0x2a, 0x2d, 0x3c);
    pub const BUTTON: Color32 = Color32::from_rgb(0x25, 0x28, 0x36);
    pub const BUTTON_HOVER: Color32 = Color32::from_rgb(0x30, 0x34, 0x46);
    pub const TEXT: Color32 = Color32::from_rgb(0xe8, 0xea, 0xf2);
    pub const DIM: Color32 = Color32::from_rgb(0x8b, 0x90, 0xa6);
    pub const FAINT: Color32 = Color32::from_rgb(0x4a, 0x4e, 0x62);
    pub const ACCENT: Color32 = Color32::from_rgb(0x94, 0x6e, 0xff);
    pub const ACCENT_DARK: Color32 = Color32::from_rgb(0x6c, 0x4b, 0xe0);
    pub const TEAL: Color32 = Color32::from_rgb(0x3d, 0xdb, 0xc4);
    pub const RED: Color32 = Color32::from_rgb(0xff, 0x53, 0x70);
    pub const AMBER: Color32 = Color32::from_rgb(0xff, 0xc1, 0x5e);
    pub const WHITE_KEY: Color32 = Color32::from_rgb(0x2c, 0x2f, 0x3e);
    pub const BLACK_KEY: Color32 = Color32::from_rgb(0x16, 0x17, 0x1f);
}
use palette::*;

/// Wraps the egui editor to remember the host's parent window handle, which the native drag and
/// drop implementation needs.
struct CaptureParent {
    inner: Box<dyn Editor>,
    parent: Arc<Mutex<Option<RawParent>>>,
}

struct SpawnGuard {
    _inner: Box<dyn Any + Send>,
    parent: Arc<Mutex<Option<RawParent>>>,
}

impl Drop for SpawnGuard {
    fn drop(&mut self) {
        *self.parent.lock() = None;
    }
}

impl Editor for CaptureParent {
    fn spawn(
        &self,
        parent: ParentWindowHandle,
        context: Arc<dyn GuiContext>,
    ) -> Box<dyn Any + Send> {
        *self.parent.lock() = Some(RawParent::from(&parent));
        Box::new(SpawnGuard {
            _inner: self.inner.spawn(parent, context),
            parent: self.parent.clone(),
        })
    }

    fn size(&self) -> (u32, u32) {
        self.inner.size()
    }

    fn set_scale_factor(&self, factor: f32) -> bool {
        self.inner.set_scale_factor(factor)
    }

    fn param_value_changed(&self, id: &str, normalized_value: f32) {
        self.inner.param_value_changed(id, normalized_value)
    }

    fn param_modulation_changed(&self, id: &str, modulation_offset: f32) {
        self.inner.param_modulation_changed(id, modulation_offset)
    }

    fn param_values_changed(&self) {
        self.inner.param_values_changed()
    }
}

struct GuiState {
    shared: Arc<Shared>,
    parent: Arc<Mutex<Option<RawParent>>>,
    /// The settings used for the current result, `None` if the clip has not been analyzed.
    analyzed_with: Option<(Settings, u64)>,
    toast: Option<(String, Color32, Instant)>,
    /// Results of file dialogs, which run on their own thread.
    dialog_result: Arc<Mutex<Option<DialogResult>>>,
    dialog_open: bool,
}

enum DialogResult {
    Open(Option<PathBuf>),
    Save(Option<PathBuf>),
}

pub fn create(egui_state: Arc<EguiState>, shared: Arc<Shared>) -> Option<Box<dyn Editor>> {
    let parent = Arc::new(Mutex::new(None));

    let state = GuiState {
        analyzed_with: None,
        shared: shared.clone(),
        parent: parent.clone(),
        toast: None,
        dialog_result: Arc::new(Mutex::new(None)),
        dialog_open: false,
    };

    let inner = create_egui_editor(
        egui_state,
        state,
        |ctx, _state| setup_style(ctx),
        |ctx, _setter, state| {
            // Only re-analyze once the user lets go of a slider
            let pointer_down = ctx.input(|i| i.pointer.any_down());
            state.poll(pointer_down);
            draw(ctx, state);
            // Meters, recording and analysis progress need continuous redraws
            ctx.request_repaint_after(Duration::from_millis(16));
        },
    )?;

    Some(Box::new(CaptureParent { inner, parent }))
}

fn setup_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    let v = &mut style.visuals;
    *v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = CARD;
    v.extreme_bg_color = SURFACE;
    v.override_text_color = Some(TEXT);
    v.selection.bg_fill = ACCENT_DARK;
    v.selection.stroke = stroke(1.0, ACCENT);
    v.slider_trailing_fill = true;
    for w in [
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(6);
    }
    v.widgets.inactive.bg_fill = BUTTON;
    v.widgets.inactive.weak_bg_fill = BUTTON;
    v.widgets.inactive.bg_stroke = Stroke::NONE;
    v.widgets.hovered.bg_fill = BUTTON_HOVER;
    v.widgets.hovered.weak_bg_fill = BUTTON_HOVER;
    v.widgets.hovered.bg_stroke = stroke(1.0, FAINT);
    v.widgets.active.bg_fill = ACCENT_DARK;
    v.widgets.active.weak_bg_fill = ACCENT_DARK;
    v.widgets.noninteractive.bg_stroke = stroke(1.0, BORDER);
    style.spacing.item_spacing = Vec2::new(8.0, 6.0);
    style.spacing.slider_width = 110.0;
    style.spacing.interact_size.y = 22.0;
    ctx.set_style(style);
}

impl GuiState {
    fn toast(&mut self, text: impl Into<String>, color: Color32) {
        self.toast = Some((text.into(), color, Instant::now()));
    }

    /// Handles finished dialogs and kicks off re-analysis when needed.
    fn poll(&mut self, pointer_down: bool) {
        let result = self.dialog_result.lock().take();
        match result {
            Some(DialogResult::Open(path)) => {
                self.dialog_open = false;
                if let Some(path) = path {
                    self.load_file(path);
                }
            }
            Some(DialogResult::Save(path)) => {
                self.dialog_open = false;
                if let Some(path) = path {
                    let path = if path.extension().is_none() {
                        path.with_extension("mid")
                    } else {
                        path
                    };
                    let bytes = self.midi_bytes();
                    match midi_file::save(&path, &bytes) {
                        Ok(()) => self.toast(format!("Saved {}", path.display()), TEAL),
                        Err(e) => self.toast(e, RED),
                    }
                }
            }
            None => {}
        }

        let shared = &self.shared;
        let status = shared.status();
        if status == Status::Recording && !shared.recording.load(Ordering::Acquire) {
            // The recorder stopped by itself (maximum length)
            self.analyzed_with = None;
        }

        // Automatically (re-)analyze whenever the clip or the analysis settings change. Settings
        // that only affect the export (quantization) don't require a new analysis.
        let settings = analysis_key(*shared.settings.read());
        let generation = shared.clip_generation.load(Ordering::Acquire);
        let busy = pointer_down
            || shared.recording.load(Ordering::Acquire)
            || shared.analyzing.load(Ordering::Acquire)
            || matches!(status, Status::Loading);
        let has_audio = !shared.clip.read().is_empty();

        if !busy && has_audio && self.analyzed_with != Some((settings, generation)) {
            self.analyzed_with = Some((settings, generation));
            start_analysis(shared.clone(), settings, generation);
        }
    }

    fn load_file(&mut self, path: PathBuf) {
        let shared = self.shared.clone();
        shared.recording.store(false, Ordering::Release);
        shared.set_status(Status::Loading, "Loading audio file...");
        std::thread::spawn(move || match audio_file::load_mono(&path) {
            Ok(decoded) => {
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "Audio".into());
                shared.replace_clip(Clip::new(name, decoded.sample_rate, decoded.samples));
                shared.clip_offset.store(0.0, Ordering::Relaxed);
                shared.set_status(Status::Idle, "");
            }
            Err(e) => shared.set_status(Status::Error, e),
        });
    }

    fn midi_bytes(&self) -> Vec<u8> {
        let notes = self.shared.notes.read().clone();
        let quantize = self.shared.settings.read().quantize;
        let bpm = self.shared.host_tempo.load(Ordering::Relaxed);
        midi_file::encode(&notes, bpm, quantize, &self.clip_name())
    }

    fn clip_name(&self) -> String {
        let name = self.shared.clip.read().name.clone();
        if name.is_empty() {
            "audiotomidi".into()
        } else {
            format!("{name} (MIDI)")
        }
    }

    fn open_dialog(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let result = self.dialog_result.clone();
        // Dialogs run on their own thread: modal dialogs on the GUI thread would re-enter the
        // windowing code from their nested event loop
        std::thread::spawn(move || {
            let path = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .set_title("Open audio file")
                    .add_filter("Audio", audio_file::SUPPORTED_EXTENSIONS)
                    .pick_file(),
            )
            .map(|f| f.path().to_path_buf());
            *result.lock() = Some(DialogResult::Open(path));
        });
    }

    fn save_dialog(&mut self) {
        if self.dialog_open {
            return;
        }
        self.dialog_open = true;
        let result = self.dialog_result.clone();
        let file_name = format!("{}.mid", midi_file::sanitize(&self.clip_name()));
        std::thread::spawn(move || {
            let path = pollster::block_on(
                rfd::AsyncFileDialog::new()
                    .set_title("Save MIDI file")
                    .set_file_name(file_name)
                    .add_filter("MIDI", &["mid", "midi"])
                    .save_file(),
            )
            .map(|f| f.path().to_path_buf());
            *result.lock() = Some(DialogResult::Save(path));
        });
    }

    fn begin_drag(&mut self) {
        let bytes = self.midi_bytes();
        let path = match midi_file::write_temp(&bytes, &self.clip_name()) {
            Ok(p) => p,
            Err(e) => return self.toast(e, RED),
        };
        let parent = *self.parent.lock();
        if let Err(e) = drag_out::start(parent, path) {
            self.toast(e, AMBER);
        }
    }
}

/// The settings that influence the analysis, with export-only settings normalized away.
fn analysis_key(mut settings: Settings) -> Settings {
    settings.quantize = Quantize::Off;
    settings
}

fn start_analysis(shared: Arc<Shared>, settings: Settings, generation: u64) {
    shared.analyzing.store(true, Ordering::Release);
    shared.set_status(Status::Analyzing, "Analyzing...");
    std::thread::spawn(move || {
        let started = Instant::now();
        let notes = {
            let clip = shared.clip.read();
            analysis::analyze(&clip.samples, clip.sample_rate, &settings)
        };
        // Throw the result away if the clip changed in the meantime
        if shared.clip_generation.load(Ordering::Acquire) == generation {
            let count = notes.len();
            shared.set_notes(notes);
            shared.set_status(
                Status::Done,
                format!(
                    "{count} note{} detected in {:.0} ms",
                    if count == 1 { "" } else { "s" },
                    started.elapsed().as_secs_f64() * 1000.0
                ),
            );
        }
        shared.analyzing.store(false, Ordering::Release);
    });
}

// ------------------------------------------------------------------------------------------------
// Layout
// ------------------------------------------------------------------------------------------------

fn draw(ctx: &egui::Context, state: &mut GuiState) {
    egui::CentralPanel::default()
        .frame(egui::Frame::NONE.fill(BG))
        .show(ctx, |ui| {
            let full = ui.max_rect().shrink(16.0);

            let header = Rect::from_min_size(full.min, Vec2::new(full.width(), 34.0));
            let footer_h = 78.0;
            let footer =
                Rect::from_min_max(Pos2::new(full.left(), full.bottom() - footer_h), full.max);
            let body = Rect::from_min_max(
                Pos2::new(full.left(), header.bottom() + 12.0),
                Pos2::new(full.right(), footer.top() - 12.0),
            );

            draw_header(ui, header, state);

            let gap = 52.0;
            let card_w = (body.width() - gap) / 2.0;
            let input = Rect::from_min_size(body.min, Vec2::new(card_w, body.height()));
            let output = Rect::from_min_size(
                Pos2::new(input.right() + gap, body.top()),
                Vec2::new(card_w, body.height()),
            );
            let bridge = Rect::from_min_max(
                Pos2::new(input.right(), body.top()),
                Pos2::new(output.left(), body.bottom()),
            );

            // Both panels share the same time axis so the notes line up with the waveform
            let duration = {
                let clip = state.shared.clip.read();
                let last_note = state
                    .shared
                    .notes
                    .read()
                    .last()
                    .map(|n| n.end)
                    .unwrap_or(0.0);
                let d = clip.duration().max(last_note);
                if state.shared.recording.load(Ordering::Relaxed) {
                    // While recording, grow in steps so the waveform doesn't squash constantly
                    (d / 10.0).ceil().max(1.0) * 10.0
                } else {
                    d.max(1.0)
                }
            };

            draw_input(ui, input, state, duration);
            draw_bridge(ui, bridge, state);
            draw_output(ui, output, state, duration);
            draw_settings(ui, footer, state);
            draw_toast(ui, full, state);
        });
}

fn card(ui: &Ui, rect: Rect) {
    let p = ui.painter();
    p.rect_filled(rect, 14.0, CARD);
    p.rect_stroke(rect, 14.0, stroke(1.0, BORDER), StrokeKind::Inside);
}

fn draw_header(ui: &mut Ui, rect: Rect, state: &GuiState) {
    let p = ui.painter();

    // Logo: a small waveform turning into a note bar
    let logo = Rect::from_min_size(rect.left_center() - Vec2::new(0.0, 14.0), Vec2::splat(28.0));
    p.rect_filled(logo, 8.0, ACCENT_DARK);
    let c = logo.center();
    let wave: Vec<Pos2> = (0..9)
        .map(|i| {
            let x = c.x - 9.0 + i as f32 * 1.6;
            let y = c.y + (i as f32 * 1.3).sin() * 5.0;
            Pos2::new(x, y)
        })
        .collect();
    p.add(Shape::line(wave, stroke(1.6, Color32::WHITE)));
    p.rect_filled(
        Rect::from_min_size(Pos2::new(c.x + 3.0, c.y - 2.0), Vec2::new(7.0, 4.0)),
        1.5,
        Color32::WHITE,
    );

    p.text(
        Pos2::new(logo.right() + 10.0, rect.center().y - 1.0),
        Align2::LEFT_CENTER,
        "Audio",
        FontId::proportional(19.0),
        TEXT,
    );
    let w = p
        .layout_no_wrap("Audio".into(), FontId::proportional(19.0), TEXT)
        .size()
        .x;
    p.text(
        Pos2::new(logo.right() + 10.0 + w + 6.0, rect.center().y - 1.0),
        Align2::LEFT_CENTER,
        "to MIDI",
        FontId::proportional(19.0),
        ACCENT,
    );

    // Status pill on the right
    let shared = &state.shared;
    let status = shared.status();
    let message = shared.message.lock().clone();
    let (dot, text) = match status {
        Status::Recording => {
            let secs = shared.clip.read().duration();
            (RED, format!("Recording  {}", format_time(secs)))
        }
        Status::Loading => (AMBER, message),
        Status::Analyzing => (AMBER, message),
        Status::Done => (TEAL, message),
        Status::Error => (RED, message),
        Status::Idle if !message.is_empty() => (DIM, message),
        Status::Idle => (FAINT, "Ready".to_string()),
    };
    let bpm = shared.host_tempo.load(Ordering::Relaxed);
    let tempo_text = format!("{bpm:.1} BPM");
    let font = FontId::proportional(13.0);
    let tempo_w = p
        .layout_no_wrap(tempo_text.clone(), font.clone(), DIM)
        .size()
        .x;
    let text_w = p.layout_no_wrap(text.clone(), font.clone(), TEXT).size().x;

    let tempo_rect = Rect::from_min_max(
        Pos2::new(rect.right() - tempo_w - 24.0, rect.top() + 4.0),
        Pos2::new(rect.right(), rect.bottom() - 4.0),
    );
    p.rect_filled(tempo_rect, 13.0, CARD);
    p.text(
        tempo_rect.center(),
        Align2::CENTER_CENTER,
        tempo_text,
        font.clone(),
        DIM,
    );

    let pill = Rect::from_min_max(
        Pos2::new(tempo_rect.left() - text_w - 46.0, rect.top() + 4.0),
        Pos2::new(tempo_rect.left() - 8.0, rect.bottom() - 4.0),
    );
    p.rect_filled(pill, 13.0, CARD);
    let pulse = if matches!(
        status,
        Status::Recording | Status::Analyzing | Status::Loading
    ) {
        let t = ui.input(|i| i.time) as f32;
        0.55 + 0.45 * (t * 5.0).sin().abs()
    } else {
        1.0
    };
    p.circle_filled(
        Pos2::new(pill.left() + 16.0, pill.center().y),
        4.5,
        dot.gamma_multiply(pulse),
    );
    p.text(
        Pos2::new(pill.left() + 28.0, pill.center().y),
        Align2::LEFT_CENTER,
        text,
        font,
        TEXT,
    );
}

fn card_title(ui: &Ui, rect: Rect, title: &str, subtitle: &str, color: Color32) {
    let p = ui.painter();
    let pos = rect.min + Vec2::new(18.0, 20.0);
    p.circle_filled(pos, 4.0, color);
    p.text(
        pos + Vec2::new(12.0, 0.0),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(14.0),
        TEXT,
    );
    p.text(
        Pos2::new(rect.right() - 18.0, pos.y),
        Align2::RIGHT_CENTER,
        subtitle,
        FontId::proportional(12.0),
        DIM,
    );
}

fn draw_input(ui: &mut Ui, rect: Rect, state: &mut GuiState, duration: f64) {
    card(ui, rect);
    let shared = state.shared.clone();
    let recording = shared.recording.load(Ordering::Acquire);

    let subtitle = {
        let clip = shared.clip.read();
        if clip.is_empty() && !recording {
            "No audio".to_string()
        } else {
            format!(
                "{}  ·  {}  ·  {:.1} kHz",
                elide(&clip.name, 22),
                format_time(clip.duration()),
                clip.sample_rate / 1000.0
            )
        }
    };
    card_title(ui, rect, "INPUT", &subtitle, TEAL);

    // Waveform
    let view = Rect::from_min_max(
        rect.min + Vec2::new(14.0, 40.0),
        Pos2::new(rect.right() - 14.0, rect.bottom() - 66.0),
    );
    ui.painter().rect_filled(view, 10.0, SURFACE);
    draw_time_grid(ui, view, duration);
    {
        let clip = shared.clip.read();
        if clip.is_empty() {
            let hint = if recording {
                "Waiting for audio... (start playback in your DAW)"
            } else {
                "Record the track this plugin sits on,\nor open an audio file"
            };
            ui.painter().text(
                view.center(),
                Align2::CENTER_CENTER,
                hint,
                FontId::proportional(13.0),
                DIM,
            );
        } else {
            draw_waveform(
                ui,
                view.shrink2(Vec2::new(6.0, 10.0)),
                &clip,
                duration,
                recording,
            );
        }
    }

    // Live input level along the bottom of the waveform view
    let level = shared.input_level.load(Ordering::Relaxed);
    let db = 20.0 * level.max(1e-6).log10();
    let norm = ((db + 60.0) / 60.0).clamp(0.0, 1.0);
    let meter = Rect::from_min_max(
        Pos2::new(view.left() + 10.0, view.bottom() - 7.0),
        Pos2::new(view.right() - 10.0, view.bottom() - 4.0),
    );
    ui.painter().rect_filled(meter, 2.0, BORDER);
    let meter_fill = Rect::from_min_max(
        meter.min,
        Pos2::new(meter.left() + meter.width() * norm, meter.bottom()),
    );
    let meter_color = if db > -3.0 {
        RED
    } else if db > -12.0 {
        AMBER
    } else {
        TEAL
    };
    ui.painter().rect_filled(meter_fill, 2.0, meter_color);

    // Buttons
    let row = Rect::from_min_max(
        Pos2::new(rect.left() + 14.0, rect.bottom() - 52.0),
        Pos2::new(rect.right() - 14.0, rect.bottom() - 14.0),
    );
    ui.scope_builder(UiBuilder::new().max_rect(row), |ui| {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let busy = matches!(shared.status(), Status::Loading);

            if recording {
                if icon_button(ui, Icon::Stop, "Stop", ButtonKind::Danger, true).clicked() {
                    shared.stop_recording();
                }
            } else if icon_button(ui, Icon::Record, "Record", ButtonKind::Normal, !busy)
                .on_hover_text("Capture the audio coming into the plugin")
                .clicked()
            {
                shared.start_recording();
                state.analyzed_with = None;
            }

            if icon_button(
                ui,
                Icon::Folder,
                "Open file",
                ButtonKind::Normal,
                !recording && !busy,
            )
            .on_hover_text("WAV, AIFF, FLAC, MP3, OGG")
            .clicked()
            {
                state.open_dialog();
            }

            let has_clip = !shared.clip.read().is_empty();
            if icon_button(
                ui,
                Icon::Clear,
                "Clear",
                ButtonKind::Ghost,
                has_clip && !recording,
            )
            .clicked()
            {
                shared.replace_clip(Clip::default());
                shared.set_status(Status::Idle, "");
                state.analyzed_with = None;
            }
        });
    });
}

fn draw_bridge(ui: &mut Ui, rect: Rect, state: &mut GuiState) {
    let c = rect.center();
    let p = ui.painter();
    let analyzing = state.shared.analyzing.load(Ordering::Relaxed);

    // Connector lines between the cards
    p.line_segment(
        [Pos2::new(rect.left(), c.y), Pos2::new(rect.right(), c.y)],
        stroke(2.0, BORDER),
    );
    let circle = Rect::from_center_size(c, Vec2::splat(38.0));
    let resp = ui.interact(circle, ui.id().with("convert"), Sense::click());
    let fill = if resp.hovered() { ACCENT } else { ACCENT_DARK };
    p.circle_filled(c, 19.0, fill);
    p.circle_stroke(c, 19.0, stroke(3.0, BG));

    if analyzing {
        // Spinner
        let t = ui.input(|i| i.time) as f32 * 6.0;
        let pts: Vec<Pos2> = (0..20)
            .map(|i| {
                let a = t + i as f32 * 0.22;
                c + Vec2::new(a.cos(), a.sin()) * 9.0
            })
            .collect();
        p.add(Shape::line(pts, stroke(2.5, Color32::WHITE)));
    } else {
        let s = stroke(2.5, Color32::WHITE);
        p.line_segment([c - Vec2::new(8.0, 0.0), c + Vec2::new(7.0, 0.0)], s);
        p.line_segment([c + Vec2::new(8.0, 0.0), c + Vec2::new(2.0, -6.0)], s);
        p.line_segment([c + Vec2::new(8.0, 0.0), c + Vec2::new(2.0, 6.0)], s);
    }

    if resp.on_hover_text("Convert again").clicked() {
        state.analyzed_with = None;
    }
}

fn draw_output(ui: &mut Ui, rect: Rect, state: &mut GuiState, duration: f64) {
    card(ui, rect);
    let shared = state.shared.clone();
    let notes = shared.notes.read().clone();

    let subtitle = if notes.is_empty() {
        "No notes yet".to_string()
    } else {
        let lo = notes.iter().map(|n| n.pitch).min().unwrap_or(60);
        let hi = notes.iter().map(|n| n.pitch).max().unwrap_or(60);
        format!(
            "{} notes  ·  {} – {}",
            notes.len(),
            note_name(lo),
            note_name(hi)
        )
    };
    card_title(ui, rect, "OUTPUT", &subtitle, ACCENT);

    let view = Rect::from_min_max(
        rect.min + Vec2::new(14.0, 40.0),
        Pos2::new(rect.right() - 14.0, rect.bottom() - 66.0),
    );
    draw_piano_roll(ui, view, &notes, duration);

    let row = Rect::from_min_max(
        Pos2::new(rect.left() + 14.0, rect.bottom() - 52.0),
        Pos2::new(rect.right() - 14.0, rect.bottom() - 14.0),
    );
    let has_notes = !notes.is_empty();

    // The drag handle takes most of the row
    let side_w = 196.0;
    let drag_rect = Rect::from_min_max(row.min, Pos2::new(row.right() - side_w, row.bottom()));
    let drag = drag_handle(ui, drag_rect, has_notes);
    if has_notes {
        if drag_out::SUPPORTED {
            if drag.drag_started() {
                state.begin_drag();
            }
        } else if drag.clicked() {
            state.save_dialog();
        }
    }

    let side = Rect::from_min_max(Pos2::new(drag_rect.right() + 8.0, row.top()), row.max);
    ui.scope_builder(UiBuilder::new().max_rect(side), |ui| {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if icon_button(ui, Icon::Save, "Save .mid", ButtonKind::Normal, has_notes).clicked() {
                state.save_dialog();
            }

            let midi_out = shared.midi_out_enabled.load(Ordering::Relaxed);
            let kind = if midi_out {
                ButtonKind::Active
            } else {
                ButtonKind::Normal
            };
            if icon_button(ui, Icon::Play, "Out", kind, true)
                .on_hover_text(
                    "Play the notes on the plugin's MIDI output in sync with the DAW \
                     transport, so they can be recorded on a MIDI track",
                )
                .clicked()
            {
                shared.midi_out_enabled.store(!midi_out, Ordering::Relaxed);
            }
        });
    });
}

fn drag_handle(ui: &mut Ui, rect: Rect, enabled: bool) -> Response {
    let sense = if enabled {
        Sense::click_and_drag()
    } else {
        Sense::hover()
    };
    let resp = ui.interact(rect, ui.id().with("drag-midi"), sense);
    let p = ui.painter();

    let (fill, text_color) = if !enabled {
        (BUTTON, FAINT)
    } else if resp.is_pointer_button_down_on() {
        (ACCENT_DARK, Color32::WHITE)
    } else if resp.hovered() {
        (ACCENT, Color32::WHITE)
    } else {
        (ACCENT_DARK, Color32::WHITE)
    };
    p.rect_filled(rect, 10.0, fill);
    if enabled {
        // Subtle highlight on the upper half
        let top = Rect::from_min_max(rect.min, Pos2::new(rect.right(), rect.center().y));
        p.rect_filled(
            top.shrink(1.0),
            CornerRadius {
                nw: 9,
                ne: 9,
                sw: 0,
                se: 0,
            },
            Color32::from_white_alpha(10),
        );
    }

    // Grip dots
    let gx = rect.left() + 18.0;
    for row in 0..3 {
        for col in 0..2 {
            p.circle_filled(
                Pos2::new(
                    gx + col as f32 * 5.0,
                    rect.center().y - 5.0 + row as f32 * 5.0,
                ),
                1.4,
                text_color.gamma_multiply(0.8),
            );
        }
    }

    let label = if drag_out::SUPPORTED {
        "Drag MIDI into your DAW"
    } else {
        "Export MIDI clip"
    };
    p.text(
        Pos2::new(rect.left() + 36.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(14.0),
        text_color,
    );

    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::Grab)
            .on_hover_text(if drag_out::SUPPORTED {
                "Hold and drag onto a MIDI track or into the piano roll"
            } else {
                "Native drag and drop is available on Windows and macOS"
            })
    } else {
        resp
    }
}

fn draw_settings(ui: &mut Ui, rect: Rect, state: &mut GuiState) {
    card(ui, rect);
    let mut settings = *state.shared.settings.read();
    let before = settings;

    let inner = rect.shrink2(Vec2::new(18.0, 10.0));
    ui.scope_builder(UiBuilder::new().max_rect(inner), |ui| {
        ui.horizontal_centered(|ui| {
            ui.spacing_mut().item_spacing.x = 22.0;

            labeled(ui, "Sensitivity", |ui| {
                ui.add(egui::Slider::new(&mut settings.sensitivity, 0.05..=0.5).show_value(false))
                    .on_hover_text("Higher values detect more notes in noisy or breathy material");
            });
            labeled(ui, "Gate", |ui| {
                ui.add(
                    egui::Slider::new(&mut settings.gate_db, -70.0..=-10.0)
                        .suffix(" dB")
                        .fixed_decimals(0),
                )
                .on_hover_text("Anything quieter than this is ignored");
            });
            labeled(ui, "Min length", |ui| {
                ui.add(
                    egui::DragValue::new(&mut settings.min_note_ms)
                        .range(10.0..=500.0)
                        .speed(1.0)
                        .suffix(" ms")
                        .fixed_decimals(0),
                );
            });
            labeled(ui, "Range", |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    let hi = settings.high_note;
                    ui.add(
                        egui::DragValue::new(&mut settings.low_note)
                            .range(12..=hi.saturating_sub(12))
                            .speed(0.2)
                            .custom_formatter(|v, _| note_name(v as u8))
                            .custom_parser(parse_note),
                    );
                    ui.label(egui::RichText::new("–").color(DIM));
                    let lo = settings.low_note;
                    ui.add(
                        egui::DragValue::new(&mut settings.high_note)
                            .range((lo + 12)..=120)
                            .speed(0.2)
                            .custom_formatter(|v, _| note_name(v as u8))
                            .custom_parser(parse_note),
                    );
                });
            });
            labeled(ui, "Transpose", |ui| {
                ui.add(
                    egui::DragValue::new(&mut settings.transpose)
                        .range(-36..=36)
                        .speed(0.15)
                        .custom_formatter(|v, _| {
                            if v > 0.0 {
                                format!("+{v:.0} st")
                            } else {
                                format!("{v:.0} st")
                            }
                        }),
                );
            });
            labeled(ui, "Quantize", |ui| {
                egui::ComboBox::from_id_salt("quantize")
                    .width(70.0)
                    .selected_text(settings.quantize.label())
                    .show_ui(ui, |ui| {
                        for q in Quantize::ALL {
                            ui.selectable_value(&mut settings.quantize, q, q.label());
                        }
                    });
            });
        });
    });

    if settings != before {
        *state.shared.settings.write() = settings;
    }
}

fn labeled(ui: &mut Ui, label: &str, add: impl FnOnce(&mut Ui)) {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.label(egui::RichText::new(label).size(11.0).color(DIM));
        add(ui);
    });
}

fn draw_toast(ui: &mut Ui, full: Rect, state: &mut GuiState) {
    let Some((text, color, at)) = state.toast.clone() else {
        return;
    };
    let age = at.elapsed().as_secs_f32();
    if age > 4.0 {
        state.toast = None;
        return;
    }
    let alpha = (4.0 - age).min(1.0);
    let p = ui.painter();
    let font = FontId::proportional(13.0);
    let galley = p.layout_no_wrap(text, font, TEXT.gamma_multiply(alpha));
    let size = galley.size() + Vec2::new(40.0, 18.0);
    let rect = Rect::from_center_size(Pos2::new(full.center().x, full.top() + 60.0), size);
    p.rect_filled(rect, 10.0, CARD.gamma_multiply(alpha.max(0.0)));
    p.rect_stroke(
        rect,
        10.0,
        stroke(1.0, color.gamma_multiply(alpha)),
        StrokeKind::Inside,
    );
    p.circle_filled(
        Pos2::new(rect.left() + 14.0, rect.center().y),
        3.5,
        color.gamma_multiply(alpha),
    );
    p.galley(
        Pos2::new(rect.left() + 26.0, rect.center().y - galley.size().y / 2.0),
        galley,
        TEXT,
    );
}

// ------------------------------------------------------------------------------------------------
// Drawing helpers
// ------------------------------------------------------------------------------------------------

fn time_to_x(rect: Rect, t: f64, duration: f64) -> f32 {
    rect.left() + (t / duration) as f32 * rect.width()
}

fn draw_time_grid(ui: &Ui, rect: Rect, duration: f64) {
    let p = ui.painter();
    let step = [0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0]
        .into_iter()
        .find(|s| duration / s <= 10.0)
        .unwrap_or(300.0);
    let mut t = step;
    while t < duration {
        let x = time_to_x(rect, t, duration);
        p.line_segment(
            [
                Pos2::new(x, rect.top() + 4.0),
                Pos2::new(x, rect.bottom() - 4.0),
            ],
            stroke(1.0, Color32::from_white_alpha(6)),
        );
        p.text(
            Pos2::new(x + 3.0, rect.top() + 4.0),
            Align2::LEFT_TOP,
            format_time(t),
            FontId::proportional(9.5),
            FAINT,
        );
        t += step;
    }
}

fn draw_waveform(ui: &Ui, rect: Rect, clip: &Clip, duration: f64, recording: bool) {
    let p = ui.painter();
    let mid = rect.center().y;
    let half = rect.height() / 2.0;
    p.line_segment(
        [Pos2::new(rect.left(), mid), Pos2::new(rect.right(), mid)],
        stroke(1.0, Color32::from_white_alpha(10)),
    );

    if clip.peaks.is_empty() || clip.sample_rate <= 0.0 {
        return;
    }

    // Normalize the display so quiet recordings are still visible
    let max = clip
        .peaks
        .iter()
        .fold(0.0f32, |m, &(lo, hi)| m.max(-lo).max(hi))
        .max(0.05);
    let gain = 0.95 / max;

    let blocks_per_sec = clip.sample_rate as f64 / crate::shared::PEAK_BLOCK as f64;
    let width = rect.width().max(1.0) as usize;
    let color = if recording { RED } else { TEAL };

    let mut mesh = egui::Mesh::default();
    for px in 0..width {
        let t0 = px as f64 / width as f64 * duration;
        let t1 = (px + 1) as f64 / width as f64 * duration;
        let b0 = (t0 * blocks_per_sec) as usize;
        let b1 = ((t1 * blocks_per_sec).ceil() as usize).max(b0 + 1);
        if b0 >= clip.peaks.len() {
            break;
        }
        let (mut lo, mut hi) = (0.0f32, 0.0f32);
        for &(l, h) in &clip.peaks[b0..b1.min(clip.peaks.len())] {
            lo = lo.min(l);
            hi = hi.max(h);
        }
        let x = rect.left() + px as f32;
        let top = mid - (hi * gain).min(1.0) * half;
        let bottom = mid - (lo * gain).max(-1.0) * half;
        let r = Rect::from_min_max(
            Pos2::new(x, top.min(mid - 0.5)),
            Pos2::new(x + 1.0, bottom.max(mid + 0.5)),
        );
        let amp = (hi - lo) * gain * 0.5;
        mesh.add_colored_rect(r, color.gamma_multiply(0.55 + 0.45 * amp.min(1.0)));
    }
    p.add(Shape::mesh(mesh));

    if recording {
        let x = time_to_x(rect, clip.duration(), duration);
        p.line_segment(
            [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
            stroke(1.5, RED),
        );
    }
}

fn is_black(note: u8) -> bool {
    matches!(note % 12, 1 | 3 | 6 | 8 | 10)
}

fn draw_piano_roll(ui: &Ui, view: Rect, notes: &[crate::shared::Note], duration: f64) {
    let p = ui.painter();
    p.rect_filled(view, 10.0, SURFACE);

    if notes.is_empty() {
        p.text(
            view.center(),
            Align2::CENTER_CENTER,
            "Detected notes will appear here",
            FontId::proportional(13.0),
            DIM,
        );
        return;
    }

    // Visible pitch range: the used notes plus some padding, at least two octaves
    let lo = notes.iter().map(|n| n.pitch).min().unwrap_or(60) as i32;
    let hi = notes.iter().map(|n| n.pitch).max().unwrap_or(60) as i32;
    let mut low = lo - 2;
    let mut high = hi + 2;
    while high - low < 24 {
        low -= 1;
        high += 1;
    }
    let low = low.clamp(0, 127);
    let high = high.clamp(low + 1, 127);
    let rows = (high - low + 1) as f32;

    let keys_w = 26.0;
    let inner = view.shrink(6.0);
    let keys = Rect::from_min_max(inner.min, Pos2::new(inner.left() + keys_w, inner.bottom()));
    let roll = Rect::from_min_max(Pos2::new(keys.right() + 4.0, inner.top()), inner.max);
    let row_h = inner.height() / rows;
    let y_of = |pitch: i32| inner.bottom() - (pitch - low + 1) as f32 * row_h;

    // Row backgrounds and keyboard
    for pitch in low..=high {
        let y = y_of(pitch);
        let row = Rect::from_min_max(
            Pos2::new(roll.left(), y),
            Pos2::new(roll.right(), y + row_h),
        );
        let key = Rect::from_min_max(
            Pos2::new(keys.left(), y),
            Pos2::new(keys.right(), y + row_h),
        );
        let black = is_black(pitch as u8);
        if black {
            p.rect_filled(row, 0.0, Color32::from_black_alpha(40));
        }
        p.rect_filled(
            key.shrink2(Vec2::new(0.0, 0.5)),
            2.0,
            if black { BLACK_KEY } else { WHITE_KEY },
        );
        if pitch % 12 == 0 {
            p.line_segment(
                [
                    Pos2::new(roll.left(), y + row_h),
                    Pos2::new(roll.right(), y + row_h),
                ],
                stroke(1.0, Color32::from_white_alpha(12)),
            );
            if row_h * 12.0 > 30.0 {
                p.text(
                    Pos2::new(key.right() - 2.0, key.center().y),
                    Align2::RIGHT_CENTER,
                    note_name(pitch as u8),
                    FontId::proportional(8.5),
                    DIM,
                );
            }
        }
    }
    draw_time_grid(ui, roll, duration);

    // Notes
    for n in notes {
        let pitch = n.pitch as i32;
        if pitch < low || pitch > high {
            continue;
        }
        let x0 = time_to_x(roll, n.start, duration);
        let x1 = time_to_x(roll, n.end, duration).max(x0 + 2.0);
        let y = y_of(pitch);
        let r = Rect::from_min_max(Pos2::new(x0, y + 0.5), Pos2::new(x1, y + row_h - 0.5));
        let v = n.velocity as f32 / 127.0;
        let color = lerp_color(ACCENT_DARK, ACCENT, v);
        p.rect_filled(r, (row_h * 0.3).min(3.0), color);
        if r.width() > 4.0 {
            p.rect_filled(
                Rect::from_min_size(r.min, Vec2::new(2.0, r.height())),
                1.0,
                Color32::from_white_alpha(90),
            );
        }
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

#[derive(Clone, Copy, PartialEq)]
enum ButtonKind {
    Normal,
    Ghost,
    Danger,
    Active,
}

#[derive(Clone, Copy)]
enum Icon {
    Record,
    Stop,
    Folder,
    Clear,
    Save,
    Play,
}

fn icon_button(ui: &mut Ui, icon: Icon, text: &str, kind: ButtonKind, enabled: bool) -> Response {
    let font = FontId::proportional(13.0);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, TEXT);
    let size = Vec2::new(galley.size().x + 44.0, 34.0);
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, resp) = ui.allocate_exact_size(size, sense);

    let hovered = enabled && resp.hovered();
    let pressed = enabled && resp.is_pointer_button_down_on();
    let (fill, fg) = match kind {
        _ if !enabled => (BUTTON.gamma_multiply(0.6), FAINT),
        ButtonKind::Normal => (
            if pressed {
                BUTTON
            } else if hovered {
                BUTTON_HOVER
            } else {
                BUTTON
            },
            TEXT,
        ),
        ButtonKind::Ghost => (
            if hovered {
                BUTTON
            } else {
                Color32::TRANSPARENT
            },
            if hovered { TEXT } else { DIM },
        ),
        ButtonKind::Danger => (
            if hovered {
                RED
            } else {
                RED.gamma_multiply(0.8)
            },
            Color32::WHITE,
        ),
        ButtonKind::Active => (
            if hovered {
                TEAL
            } else {
                TEAL.gamma_multiply(0.85)
            },
            BG,
        ),
    };

    let p = ui.painter();
    p.rect_filled(rect, 9.0, fill);
    if kind == ButtonKind::Ghost && enabled {
        p.rect_stroke(rect, 9.0, stroke(1.0, BORDER), StrokeKind::Inside);
    }

    let ic = Pos2::new(rect.left() + 17.0, rect.center().y);
    let icon_color = match (icon, kind, enabled) {
        (Icon::Record, ButtonKind::Normal, true) => RED,
        _ => fg,
    };
    paint_icon(p, icon, ic, icon_color, ui.input(|i| i.time) as f32);
    p.galley(
        Pos2::new(rect.left() + 31.0, rect.center().y - galley.size().y / 2.0),
        galley,
        fg,
    );

    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

fn paint_icon(p: &egui::Painter, icon: Icon, c: Pos2, color: Color32, time: f32) {
    let s = stroke(1.6, color);
    match icon {
        Icon::Record => {
            p.circle_filled(c, 5.0, color);
        }
        Icon::Stop => {
            let pulse = 0.75 + 0.25 * (time * 5.0).sin().abs();
            p.rect_filled(
                Rect::from_center_size(c, Vec2::splat(9.0)),
                2.0,
                color.gamma_multiply(pulse),
            );
        }
        Icon::Folder => {
            let body = Rect::from_center_size(c + Vec2::new(0.0, 1.0), Vec2::new(13.0, 9.0));
            p.rect_stroke(body, 2.0, s, StrokeKind::Middle);
            p.line_segment(
                [
                    body.left_top() + Vec2::new(1.0, -2.0),
                    body.left_top() + Vec2::new(5.0, -2.0),
                ],
                s,
            );
        }
        Icon::Clear => {
            let d = 4.0;
            p.line_segment([c - Vec2::splat(d), c + Vec2::splat(d)], s);
            p.line_segment([c + Vec2::new(-d, d), c + Vec2::new(d, -d)], s);
        }
        Icon::Save => {
            p.line_segment([c - Vec2::new(0.0, 6.0), c + Vec2::new(0.0, 2.0)], s);
            p.line_segment([c + Vec2::new(0.0, 2.0), c + Vec2::new(-3.5, -1.5)], s);
            p.line_segment([c + Vec2::new(0.0, 2.0), c + Vec2::new(3.5, -1.5)], s);
            p.line_segment([c + Vec2::new(-6.0, 5.5), c + Vec2::new(6.0, 5.5)], s);
        }
        Icon::Play => {
            p.add(Shape::convex_polygon(
                vec![
                    c + Vec2::new(-3.5, -5.5),
                    c + Vec2::new(5.5, 0.0),
                    c + Vec2::new(-3.5, 5.5),
                ],
                color,
                Stroke::NONE,
            ));
        }
    }
}

// ------------------------------------------------------------------------------------------------
// Formatting
// ------------------------------------------------------------------------------------------------

const NOTE_NAMES: [&str; 12] = [
    "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
];

pub fn note_name(note: u8) -> String {
    format!("{}{}", NOTE_NAMES[note as usize % 12], note as i32 / 12 - 1)
}

fn parse_note(text: &str) -> Option<f64> {
    let text = text.trim();
    if let Ok(n) = text.parse::<f64>() {
        return Some(n);
    }
    let upper = text.to_uppercase();
    let (name, octave) = upper.split_at(upper.find(|c: char| c == '-' || c.is_ascii_digit())?);
    let idx = NOTE_NAMES.iter().position(|&n| n == name)? as i32;
    let octave: i32 = octave.parse().ok()?;
    Some(((octave + 1) * 12 + idx).clamp(0, 127) as f64)
}

fn format_time(secs: f64) -> String {
    let secs = secs.max(0.0);
    let m = (secs / 60.0).floor() as u64;
    let s = secs - m as f64 * 60.0;
    if m > 0 {
        format!("{m}:{:04.1}", s)
    } else {
        format!("{s:.1}s")
    }
}

fn elide(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max - 1).collect();
        s.push('…');
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn note_names() {
        assert_eq!(note_name(60), "C4");
        assert_eq!(note_name(69), "A4");
        assert_eq!(parse_note("A4"), Some(69.0));
        assert_eq!(parse_note("c#3"), Some(49.0));
    }
}

fn stroke(width: f32, color: Color32) -> Stroke {
    Stroke::new(width, color)
}
