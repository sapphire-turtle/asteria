//! Compares pure-Rust Opus decoders against libopus on an Ogg Opus file.
//!
//! Throwaway spike code: it answers "which pure-Rust decoder can the iOS app
//! use" with SNR against libopus, speed and a PCM hash that can be compared
//! between the phone and the desktop.

use std::io::Cursor;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

/// A 10 s stereo sample (sine + pink noise, 96 kbit/s), made by `fixtures/make.sh`.
pub const SAMPLE: &[u8] = include_bytes!("../fixtures/sample.opus");

/// Opus always decodes at 48 kHz here, and the granule position counts 48 kHz samples.
const SAMPLE_RATE: u32 = 48_000;
/// The longest Opus packet (120 ms) in samples per channel at 48 kHz.
const MAX_PACKET_SAMPLES: usize = 5760;

/// Why reading, decoding or the reference run failed.
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    /// The bytes are not a readable Ogg stream.
    #[error("ogg: {0}")]
    Ogg(String),
    /// The Ogg stream is not a supported Ogg Opus stream.
    #[error("not a supported Ogg Opus stream: {0}")]
    NotOpus(String),
    /// The candidate cannot decode this input at all.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// The decoder returned an error or panicked.
    #[error("decode: {0}")]
    Decode(String),
    /// The ffmpeg (libopus) reference could not be produced.
    #[error("reference: {0}")]
    Reference(String),
}

/// The parts of an Ogg Opus file the decoders need.
#[derive(Debug, Clone)]
pub struct OggOpus {
    /// Channel count from `OpusHead` (1 or 2; mapping family 0 only).
    pub channels: u8,
    /// Samples (48 kHz, per channel) to drop from the start of the decoded output.
    pub pre_skip: u16,
    /// Audio packets, after `OpusHead` and `OpusTags`.
    pub packets: Vec<Vec<u8>>,
    /// Granule position of the last page: total 48 kHz samples per channel, including pre-skip.
    pub final_granule: u64,
}

impl OggOpus {
    /// Samples per channel left after dropping the pre-skip.
    fn frames_after_pre_skip(&self) -> u64 {
        self.final_granule.saturating_sub(u64::from(self.pre_skip))
    }
}

/// Reads an Ogg Opus stream into [`OggOpus`].
pub fn read_ogg_opus(bytes: &[u8]) -> Result<OggOpus, ProbeError> {
    let mut reader = ogg::PacketReader::new(Cursor::new(bytes));
    let mut packets: Vec<Vec<u8>> = Vec::new();
    let mut head: Option<(u8, u16)> = None;
    let mut final_granule = 0;
    let mut index = 0usize;
    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| ProbeError::Ogg(e.to_string()))?
    {
        // Every packet of the last page carries that page's granule position.
        final_granule = packet.absgp_page();
        match index {
            0 => head = Some(parse_head(&packet.data)?),
            1 => {} // OpusTags
            _ => packets.push(packet.data),
        }
        index += 1;
    }
    let (channels, pre_skip) =
        head.ok_or_else(|| ProbeError::NotOpus("no packets in the stream".into()))?;
    if packets.is_empty() {
        return Err(ProbeError::NotOpus("no audio packets".into()));
    }
    Ok(OggOpus {
        channels,
        pre_skip,
        packets,
        final_granule,
    })
}

/// Parses `OpusHead` into `(channels, pre_skip)`.
fn parse_head(data: &[u8]) -> Result<(u8, u16), ProbeError> {
    if data.len() < 19 || &data[..8] != b"OpusHead" {
        return Err(ProbeError::NotOpus("first packet is not OpusHead".into()));
    }
    let channels = data[9];
    let pre_skip = u16::from_le_bytes([data[10], data[11]]);
    let family = data[18];
    if family != 0 || !(1..=2).contains(&channels) {
        return Err(ProbeError::NotOpus(format!(
            "channel mapping family {family} with {channels} channels"
        )));
    }
    Ok((channels, pre_skip))
}

/// A pure-Rust decoder under comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Candidate {
    /// The `opus-decoder` crate.
    OpusDecoder,
    /// The `opuscule` crate.
    Opuscule,
    /// The `opus-pure` crate.
    OpusPure,
}

impl Candidate {
    /// Every candidate, in report order.
    pub const ALL: [Candidate; 3] = [
        Candidate::OpusDecoder,
        Candidate::Opuscule,
        Candidate::OpusPure,
    ];

    /// The crate name.
    pub fn name(self) -> &'static str {
        match self {
            Candidate::OpusDecoder => "opus-decoder",
            Candidate::Opuscule => "opuscule",
            Candidate::OpusPure => "opus-pure",
        }
    }

    /// The crate's license (SPDX), as declared on crates.io.
    pub fn license(self) -> &'static str {
        match self {
            Candidate::OpusDecoder => "MIT OR Apache-2.0",
            Candidate::Opuscule => "MPL-2.0",
            Candidate::OpusPure => "BSD-3-Clause",
        }
    }
}

/// Decodes every packet with `c` into interleaved f32 at 48 kHz, drops the
/// pre-skip and cuts to `(final_granule - pre_skip) * channels` samples.
pub fn decode(c: Candidate, s: &OggOpus) -> Result<Vec<f32>, ProbeError> {
    let channels = usize::from(s.channels);
    if s.final_granule < u64::from(s.pre_skip) {
        return Err(ProbeError::Decode("final granule is below pre-skip".into()));
    }
    let mut pcm = match c {
        Candidate::OpusDecoder => decode_opus_decoder(s, channels)?,
        Candidate::Opuscule => decode_opuscule(s, channels)?,
        Candidate::OpusPure => decode_opus_pure(s, channels)?,
    };
    pcm.drain(..(usize::from(s.pre_skip) * channels).min(pcm.len()));
    let wanted = usize::try_from(s.frames_after_pre_skip()).unwrap_or(usize::MAX);
    pcm.truncate(wanted.saturating_mul(channels));
    Ok(pcm)
}

/// `opus-decoder`: `decode_float(packet, pcm, fec)`; its core decodes to i16
/// and scales to f32, so its output is 16-bit quantised.
fn decode_opus_decoder(s: &OggOpus, channels: usize) -> Result<Vec<f32>, ProbeError> {
    let mut dec = opus_decoder::OpusDecoder::new(SAMPLE_RATE, channels)
        .map_err(|e| ProbeError::Decode(e.to_string()))?;
    let mut out = Vec::new();
    let mut buf = vec![0f32; MAX_PACKET_SAMPLES * channels];
    for p in &s.packets {
        let n = dec
            .decode_float(p, &mut buf, false)
            .map_err(|e| ProbeError::Decode(e.to_string()))?;
        out.extend_from_slice(&buf[..n * channels]);
    }
    Ok(out)
}

/// `opuscule`: `Decoder::decode(Some(packet), pcm, fec)` with the native
/// sample type (`f32` in its default float build).
fn decode_opuscule(s: &OggOpus, channels: usize) -> Result<Vec<f32>, ProbeError> {
    let ch = opuscule::Channels::from_count(channels)
        .ok_or_else(|| ProbeError::Unsupported(format!("{channels} channels")))?;
    let mut dec = opuscule::Decoder::new(opuscule::SampleRate::Hz48000, ch);
    let mut out = Vec::new();
    let mut buf: Vec<opuscule::Val> = vec![Default::default(); MAX_PACKET_SAMPLES * channels];
    for p in &s.packets {
        let n = dec
            .decode(Some(p), &mut buf, false)
            .map_err(|e| ProbeError::Decode(e.to_string()))?;
        out.extend_from_slice(&buf[..n * channels]);
    }
    Ok(out)
}

/// `opus-pure`: `OpusDecoder::decode(packet, frame_size, pcm)` to f32.
fn decode_opus_pure(s: &OggOpus, channels: usize) -> Result<Vec<f32>, ProbeError> {
    let mut dec = opus_pure::OpusDecoder::new(SAMPLE_RATE as i32, channels)
        .map_err(|e| ProbeError::Decode(e.to_string()))?;
    let mut out = Vec::new();
    let mut buf = vec![0f32; MAX_PACKET_SAMPLES * channels];
    for p in &s.packets {
        let n = dec
            .decode(p, MAX_PACKET_SAMPLES, &mut buf)
            .map_err(|e| ProbeError::Decode(e.to_string()))?;
        out.extend_from_slice(&buf[..n * channels]);
    }
    Ok(out)
}

/// Signal-to-noise ratio of `test` against `reference`, in dB, over the
/// shorter length. Returns infinity when the error is exactly zero.
pub fn snr_db(reference: &[f32], test: &[f32]) -> f64 {
    let (mut signal, mut noise) = (0f64, 0f64);
    for (r, t) in reference.iter().zip(test) {
        let (r, t) = (f64::from(*r), f64::from(*t));
        signal += r * r;
        noise += (r - t) * (r - t);
    }
    if noise == 0.0 {
        f64::INFINITY
    } else {
        10.0 * (signal / noise).log10()
    }
}

/// `blake3:<hex>` of the samples as little-endian f32 bytes, to compare the
/// output of the phone and the desktop.
pub fn pcm_hash(pcm: &[f32]) -> String {
    let mut hasher = blake3::Hasher::new();
    for s in pcm {
        hasher.update(&s.to_le_bytes());
    }
    format!("blake3:{}", hasher.finalize().to_hex())
}

/// One candidate's result.
#[derive(Debug, Clone)]
pub struct Measurement {
    /// Which decoder was measured.
    pub candidate: Candidate,
    /// Length of the decoded audio in seconds.
    pub audio_secs: f64,
    /// Wall-clock time spent decoding.
    pub decode_secs: f64,
    /// `audio_secs / decode_secs`.
    pub x_realtime: f64,
    /// SNR against the reference, when one was given.
    pub snr_db: Option<f64>,
    /// Hash of the decoded PCM; `None` on failure.
    pub pcm_hash: Option<String>,
    /// Failure (including a caught panic); `None` on success.
    pub error: Option<String>,
}

/// Decodes with `c`, timing it and comparing with `reference`. Errors and
/// panics land in `error`.
pub fn measure(c: Candidate, s: &OggOpus, reference: Option<&[f32]>) -> Measurement {
    let start = Instant::now();
    let result = catch_unwind(AssertUnwindSafe(|| decode(c, s)))
        .unwrap_or_else(|p| Err(ProbeError::Decode(format!("panic: {}", panic_text(&*p)))));
    let decode_secs = start.elapsed().as_secs_f64();
    let mut m = Measurement {
        candidate: c,
        audio_secs: s.frames_after_pre_skip() as f64 / f64::from(SAMPLE_RATE),
        decode_secs,
        x_realtime: 0.0,
        snr_db: None,
        pcm_hash: None,
        error: None,
    };
    match result {
        Ok(pcm) => {
            m.x_realtime = m.audio_secs / decode_secs.max(f64::EPSILON);
            m.snr_db = reference.map(|r| snr_db(r, &pcm));
            m.pcm_hash = Some(pcm_hash(&pcm));
        }
        Err(e) => m.error = Some(e.to_string()),
    }
    m
}

/// The message of a caught panic payload.
fn panic_text(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown".into())
}

/// Decodes `file` with ffmpeg's libopus (`-c:a libopus` on the input side, so
/// ffmpeg's own Opus decoder is not used) into interleaved f32 at 48 kHz.
/// ffmpeg applies the pre-skip and end trimming itself.
pub fn libopus_reference(file: &Path) -> Result<Vec<f32>, ProbeError> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-c:a", "libopus", "-i"])
        .arg(file)
        .args(["-f", "f32le", "-ar", "48000", "-"])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| ProbeError::Reference(format!("cannot run ffmpeg: {e}")))?;
    if !out.status.success() {
        return Err(ProbeError::Reference(format!(
            "ffmpeg failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out
        .stdout
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect())
}
