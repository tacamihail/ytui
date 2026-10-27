//! symphonia wrapper: opens a googlevideo URL, decodes packets and hands back
//! interleaved stereo f32 at the output rate.

use std::io;

use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

use crate::audio::http::HttpFile;
use crate::audio::resample::Resampler;
use crate::audio::OUT_RATE;

pub struct Loaded {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    resampler: Option<Resampler>,
    /// Sample rate of the samples the decoder actually returns. For HE-AAC
    /// this is the AAC core rate (usually 22050), not the container rate
    /// (44100): symphonia does not apply SBR, so using the container rate
    /// would play the track at double speed.
    decoded_rate: Option<u32>,
    stereo: Vec<[f32; 2]>,
    /// Length of the stream in seconds (0 if unknown).
    pub duration: f64,
}

impl Loaded {
    pub fn open(url: &str, chunk_bytes: usize) -> Result<Self, String> {
        let src = HttpFile::open(url, chunk_bytes).map_err(|e| format!("open stream: {e}"))?;
        let mss = MediaSourceStream::new(Box::new(src), Default::default());

        let mut hint = Hint::new();
        hint.with_extension("m4a");

        let probed = symphonia::default::get_probe()
            .format(
                &hint,
                mss,
                &FormatOptions {
                    enable_gapless: true,
                    ..Default::default()
                },
                &MetadataOptions::default(),
            )
            .map_err(|e| format!("unsupported stream: {e}"))?;

        let format = probed.format;
        let track = format.default_track().ok_or("stream has no audio track")?;
        let track_id = track.id;
        let params = &track.codec_params;
        let rate = params.sample_rate.unwrap_or(OUT_RATE);
        let duration = params
            .n_frames
            .map(|n| n as f64 / f64::from(rate))
            .unwrap_or(0.0);

        let decoder = symphonia::default::get_codecs()
            .make(params, &DecoderOptions::default())
            .map_err(|e| format!("unsupported codec: {e}"))?;

        Ok(Self {
            format,
            decoder,
            track_id,
            resampler: None,
            decoded_rate: None,
            stereo: Vec::with_capacity(8192),
            duration,
        })
    }

    /// Decodes the next packet into `out` (interleaved stereo f32 @ 48 kHz).
    /// Returns `false` at end of stream.
    pub fn next(&mut self, out: &mut Vec<f32>) -> Result<bool, String> {
        loop {
            let packet = match self.format.next_packet() {
                Ok(p) => p,
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(false)
                }
                Err(SymError::ResetRequired) => return Ok(false),
                Err(e) => return Err(format!("read: {e}")),
            };
            if packet.track_id() != self.track_id {
                continue;
            }
            let decoded = match self.decoder.decode(&packet) {
                Ok(d) => d,
                // A corrupt packet mid-stream should not kill playback.
                Err(SymError::DecodeError(_)) => continue,
                Err(e) => return Err(format!("decode: {e}")),
            };

            let spec = *decoded.spec();
            // The decoder decides the real rate (HE-AAC decodes at its core
            // rate), so build/refresh the resampler from the decoded spec.
            if self.decoded_rate != Some(spec.rate) {
                self.resampler = if spec.rate == OUT_RATE {
                    None
                } else {
                    Some(Resampler::new(spec.rate, OUT_RATE))
                };
                self.decoded_rate = Some(spec.rate);
            }
            let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
            buf.copy_interleaved_ref(decoded);
            let samples = buf.samples();
            let channels = spec.channels.count().max(1);

            match &mut self.resampler {
                None => push_stereo(samples, channels, out),
                Some(rs) => {
                    self.stereo.clear();
                    self.stereo.reserve(samples.len() / channels);
                    for frame in samples.chunks_exact(channels) {
                        self.stereo.push(match channels {
                            1 => [frame[0], frame[0]],
                            2 => [frame[0], frame[1]],
                            _ => [frame[0], frame[1]],
                        });
                    }
                    rs.process(&self.stereo, out);
                }
            }
            return Ok(true);
        }
    }

    pub fn seek(&mut self, secs: f64) -> Result<(), String> {
        let to = SeekTo::Time {
            time: Time::from(secs.max(0.0)),
            track_id: Some(self.track_id),
        };
        self.format
            .seek(SeekMode::Accurate, to)
            .map_err(|e| format!("seek: {e}"))?;
        self.decoder.reset();
        self.resampler = None;
        self.decoded_rate = None;
        Ok(())
    }
}

fn push_stereo(samples: &[f32], channels: usize, out: &mut Vec<f32>) {
    match channels {
        1 => {
            for &s in samples {
                out.push(s);
                out.push(s);
            }
        }
        2 => out.extend_from_slice(samples),
        _ => {
            for frame in samples.chunks_exact(channels) {
                out.push(frame[0]);
                out.push(frame[1]);
            }
        }
    }
}
