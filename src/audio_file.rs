//! Decoding audio files (WAV, AIFF, FLAC, MP3, OGG Vorbis) into a mono sample buffer.

use std::fs::File;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const SUPPORTED_EXTENSIONS: &[&str] = &["wav", "wave", "aif", "aiff", "flac", "mp3", "ogg"];

pub struct Decoded {
    pub sample_rate: f32,
    pub samples: Vec<f32>,
}

pub fn load_mono(path: &Path) -> Result<Decoded, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open file: {e}"))?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());

    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }

    let probed = symphonia::default::get_probe()
        .format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .map_err(|e| format!("Unsupported format: {e}"))?;
    let mut format = probed.format;

    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or("No audio track found")?;
    let track_id = track.id;
    let sample_rate = track
        .codec_params
        .sample_rate
        .ok_or("Unknown sample rate")? as f32;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| format!("Unsupported codec: {e}"))?;

    let mut samples = Vec::new();
    let mut buf: Option<SampleBuffer<f32>> = None;
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break
            }
            Err(SymphoniaError::ResetRequired) => break,
            Err(e) => return Err(format!("Read error: {e}")),
        };
        if packet.track_id() != track_id {
            continue;
        }

        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            // Corrupt frames are skipped
            Err(SymphoniaError::DecodeError(_)) => continue,
            Err(e) => return Err(format!("Decode error: {e}")),
        };

        let spec = *decoded.spec();
        let channels = spec.channels.count().max(1);
        let needed = decoded.capacity() as u64;
        if buf
            .as_ref()
            .map_or(true, |b| b.capacity() < needed as usize * channels)
        {
            buf = Some(SampleBuffer::new(needed, spec));
        }
        let b = buf.as_mut().unwrap();
        b.copy_interleaved_ref(decoded);
        for frame in b.samples().chunks(channels) {
            samples.push(frame.iter().sum::<f32>() / channels as f32);
        }
    }

    if samples.is_empty() {
        return Err("The file contains no audio".into());
    }
    Ok(Decoded {
        sample_rate,
        samples,
    })
}
