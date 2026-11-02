//! The offline-audio preset made in the server: Ogg Opus at 96 kbit/s, without tags or
//! cover art, from the first audio track of a file.
//!
//! symphonia decodes the library's formats (FLAC, MP3, AAC and ALAC in MP4, Vorbis, WAV) and
//! opus-pure decodes Ogg Opus, which symphonia cannot. Mono stays mono, stereo stays stereo,
//! and more channels are mixed down to stereo (see [`downmix_weights`]). rubato resamples to
//! 48 kHz when the source has another rate, libopus (the `opus` crate) encodes 20 ms frames,
//! and the `ogg` crate puts them in pages as RFC 7845 lays out.

use std::fs::File;
use std::io::{self, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use ogg::{PacketReader, PacketWriteEndInfo, PacketWriter};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::audio::{Channels, Position};
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;

/// Why an encode did not finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EncodeError {
    /// The cancel flag was set.
    Cancelled,
    /// The source cannot be decoded; retrying will not help.
    Unprocessable(String),
    /// Writing the output failed.
    Io(String),
}

/// Opus works at 48 kHz, and granule positions count 48 kHz samples.
const OPUS_RATE: u32 = 48_000;
/// Samples per channel in one 20 ms frame.
const FRAME: usize = 960;
const BITRATE: i32 = 96_000;
/// Room for one encoded packet; libopus recommends 4000 bytes.
const MAX_PACKET: usize = 4000;
/// Packets per Ogg page: one second, so a player can start or seek without reading far.
const PACKETS_PER_PAGE: u64 = 50;
/// The vendor string in `OpusTags`.
const VENDOR: &str = "asteria";
/// The serial number of the one logical stream.
const SERIAL: u32 = 0x6173_7472;
/// Decode errors in a row that are skipped before the file counts as damaged.
const MAX_BAD_PACKETS: u32 = 10;
/// Input frames the resampler takes per call.
const RESAMPLE_CHUNK: usize = 1024;

/// Encodes the first audio track of `src` to Ogg Opus at `dest`. `cancel` is checked for
/// every frame; once it is set the encode stops with [`EncodeError::Cancelled`]. `dest` is
/// left only on success.
pub(crate) fn encode_offline_audio(
    src: &Path,
    dest: &Path,
    cancel: &AtomicBool,
) -> Result<(), EncodeError> {
    let result = encode(src, dest, cancel);
    if result.is_err() {
        let _ = std::fs::remove_file(dest);
    }
    result
}

fn encode(src: &Path, dest: &Path, cancel: &AtomicBool) -> Result<(), EncodeError> {
    check(cancel)?;
    let file = File::open(src)
        .map_err(|e| EncodeError::Unprocessable(format!("cannot open the source: {e}")))?;
    let ext = src.extension().and_then(|e| e.to_str());
    let mut source = open_source(file, ext)?;
    let in_channels = source.channels();
    let out_channels = in_channels.min(2);
    let weights = (in_channels > 2).then(|| downmix_weights(&source.layout(), in_channels));
    let mut resampler = match source.rate() {
        OPUS_RATE => None,
        rate => Some(Resampling::new(rate, out_channels)?),
    };
    let out = File::create(dest).map_err(|e| EncodeError::Io(format!("cannot create: {e}")))?;
    let mut muxer = OggOpus::new(BufWriter::new(out), out_channels, source.input_rate())?;
    let mut decoded = false;
    while let Some(block) = source.next_block()? {
        check(cancel)?;
        decoded |= !block.is_empty();
        let mixed = match &weights {
            Some(w) => downmix(&block, w),
            None => block,
        };
        match &mut resampler {
            Some(r) => {
                let out = r.process(&mixed)?;
                muxer.push(&out, cancel)?;
            }
            None => muxer.push(&mixed, cancel)?,
        }
    }
    if !decoded {
        return Err(EncodeError::Unprocessable("no audio in the file".into()));
    }
    if let Some(r) = &mut resampler {
        let out = r.flush()?;
        muxer.push(&out, cancel)?;
    }
    muxer.finish(cancel)
}

fn check(cancel: &AtomicBool) -> Result<(), EncodeError> {
    if cancel.load(Ordering::Relaxed) {
        Err(EncodeError::Cancelled)
    } else {
        Ok(())
    }
}

fn unprocessable(what: &str, e: impl std::fmt::Display) -> EncodeError {
    EncodeError::Unprocessable(format!("{what}: {e}"))
}

// ------------------------------------------------------------------------------ decoding

/// Decoded audio, block by block.
trait Source {
    /// The rate of the decoded samples.
    fn rate(&self) -> u32;
    /// The rate the audio originally had, for `OpusHead`.
    fn input_rate(&self) -> u32 {
        self.rate()
    }
    fn channels(&self) -> usize;
    /// What the channels are, for mixing down.
    fn layout(&self) -> Channels;
    /// The next block of interleaved samples; `None` at the end.
    fn next_block(&mut self) -> Result<Option<Vec<f32>>, EncodeError>;
}

/// Ogg Opus (an `OggS` page whose first packet starts `OpusHead`) through opus-pure, anything
/// else through symphonia with `ext` as its hint.
fn open_source(mut file: File, ext: Option<&str>) -> Result<Box<dyn Source>, EncodeError> {
    let opus = is_ogg_opus(&mut file).map_err(|e| unprocessable("cannot read the source", e))?;
    if opus {
        Ok(Box::new(OpusSource::open(file)?))
    } else {
        Ok(Box::new(SymphoniaSource::open(file, ext)?))
    }
}

fn is_ogg_opus(file: &mut File) -> io::Result<bool> {
    let mut head = Vec::with_capacity(27 + 255 + 8);
    file.take(27 + 255 + 8).read_to_end(&mut head)?;
    file.seek(SeekFrom::Start(0))?;
    if head.len() < 27 || &head[..4] != b"OggS" {
        return Ok(false);
    }
    let body = 27 + usize::from(head[26]);
    Ok(head.get(body..body + 8) == Some(b"OpusHead"))
}

struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    rate: u32,
    channels: usize,
    layout: Channels,
    /// Frames still to give out when the container counts them: a decoder that does not
    /// trim the encoder's padding gives more, and the rest is cut.
    remaining: Option<u64>,
    /// The first block, decoded to learn the rate and the channels.
    peeked: Option<Vec<f32>>,
}

impl SymphoniaSource {
    fn open(file: File, ext: Option<&str>) -> Result<Self, EncodeError> {
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = ext {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(|e| unprocessable("unrecognised format", e))?;
        // The first audio track, as ffmpeg's `-map 0:a:0`.
        let (track_id, params, num_frames) = format
            .tracks()
            .iter()
            .find_map(|t| {
                let params = t.codec_params.as_ref()?.audio()?;
                Some((t.id, params.clone(), t.num_frames))
            })
            .ok_or_else(|| EncodeError::Unprocessable("no audio track".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(&params, &AudioDecoderOptions::default())
            .map_err(|e| unprocessable("cannot decode the audio", e))?;
        let mut s = SymphoniaSource {
            format,
            decoder,
            track_id,
            rate: 0,
            channels: 0,
            layout: Channels::None,
            remaining: num_frames,
            peeked: None,
        };
        // The decoder's first block tells the rate and the channels for certain.
        s.peeked = s.decode_next()?;
        if s.peeked.is_none() || s.rate == 0 || s.channels == 0 {
            return Err(EncodeError::Unprocessable("no audio in the file".into()));
        }
        Ok(s)
    }

    /// Decodes the next packet of the track that decodes.
    fn decode_next(&mut self) -> Result<Option<Vec<f32>>, EncodeError> {
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => return Err(unprocessable("cannot read the audio", e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    let spec = buf.spec();
                    let channels = spec.channels().count();
                    if self.channels != 0 && (spec.rate() != self.rate || channels != self.channels)
                    {
                        return Err(EncodeError::Unprocessable(
                            "the audio changes format midway".into(),
                        ));
                    }
                    self.rate = spec.rate();
                    self.channels = channels;
                    self.layout = spec.channels().clone();
                    let mut samples = Vec::new();
                    buf.copy_to_vec_interleaved(&mut samples);
                    return Ok(Some(samples));
                }
                Err(SymError::DecodeError(m)) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        return Err(unprocessable("damaged audio", m));
                    }
                }
                Err(e) => return Err(unprocessable("cannot decode the audio", e)),
            }
        }
    }
}

impl Source for SymphoniaSource {
    fn rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn layout(&self) -> Channels {
        self.layout.clone()
    }

    fn next_block(&mut self) -> Result<Option<Vec<f32>>, EncodeError> {
        if self.remaining == Some(0) {
            return Ok(None);
        }
        let block = match self.peeked.take() {
            Some(b) => Some(b),
            None => self.decode_next()?,
        };
        let Some(mut block) = block else {
            return Ok(None);
        };
        if let Some(left) = &mut self.remaining {
            let frames = (block.len() / self.channels) as u64;
            if frames > *left {
                block.truncate(*left as usize * self.channels);
            }
            *left = left.saturating_sub(frames);
        }
        Ok(Some(block))
    }
}

struct OpusSource {
    reader: PacketReader<BufReader<File>>,
    decoder: opus_pure::OpusDecoder,
    serial: u32,
    channels: usize,
    input_rate: u32,
    pre_skip: u64,
    /// Linear gain from the header's output gain.
    gain: f32,
    /// Granule position of the next sample to decode (counting the pre-skip).
    cursor: u64,
    buf: Vec<f32>,
    /// Samples per channel of the last packet that decoded.
    last_len: usize,
    ended: bool,
}

impl OpusSource {
    fn open(file: File) -> Result<Self, EncodeError> {
        let mut reader = PacketReader::new(BufReader::new(file));
        let head = reader
            .read_packet()
            .map_err(|e| unprocessable("damaged Ogg", e))?
            .ok_or_else(|| EncodeError::Unprocessable("no OpusHead".into()))?;
        let h = &head.data;
        if h.len() < 19 || &h[..8] != b"OpusHead" {
            return Err(EncodeError::Unprocessable("no OpusHead".into()));
        }
        let channels = usize::from(h[9]);
        let pre_skip = u64::from(u16::from_le_bytes([h[10], h[11]]));
        let input_rate = u32::from_le_bytes([h[12], h[13], h[14], h[15]]);
        let gain_q8 = i16::from_le_bytes([h[16], h[17]]);
        let family = h[18];
        if family != 0 || !(1..=2).contains(&channels) {
            return Err(EncodeError::Unprocessable(format!(
                "Opus channel mapping family {family} with {channels} channels is not handled"
            )));
        }
        let serial = head.stream_serial();
        // OpusTags.
        loop {
            let p = reader
                .read_packet()
                .map_err(|e| unprocessable("damaged Ogg", e))?
                .ok_or_else(|| EncodeError::Unprocessable("no OpusTags".into()))?;
            if p.stream_serial() == serial {
                break;
            }
        }
        let decoder = opus_pure::OpusDecoder::new(OPUS_RATE as i32, channels)
            .map_err(|e| unprocessable("cannot decode Opus", e))?;
        Ok(OpusSource {
            reader,
            decoder,
            serial,
            channels,
            input_rate: if input_rate == 0 {
                OPUS_RATE
            } else {
                input_rate
            },
            pre_skip,
            gain: 10f32.powf(f32::from(gain_q8) / (256.0 * 20.0)),
            cursor: 0,
            buf: vec![0.0; opus_pure::MAX_PACKET_SAMPLES * channels],
            last_len: FRAME,
            ended: false,
        })
    }
}

impl Source for OpusSource {
    fn rate(&self) -> u32 {
        OPUS_RATE
    }

    fn input_rate(&self) -> u32 {
        self.input_rate
    }

    fn channels(&self) -> usize {
        self.channels
    }

    fn layout(&self) -> Channels {
        Channels::Discrete(self.channels as u16)
    }

    fn next_block(&mut self) -> Result<Option<Vec<f32>>, EncodeError> {
        let mut bad = 0;
        loop {
            if self.ended {
                return Ok(None);
            }
            let Some(p) = self
                .reader
                .read_packet()
                .map_err(|e| unprocessable("damaged Ogg", e))?
            else {
                return Ok(None);
            };
            if p.stream_serial() != self.serial {
                continue;
            }
            let last = p.last_in_stream();
            self.ended = last;
            let n = match self
                .decoder
                .decode(&p.data, opus_pure::MAX_PACKET_SAMPLES, &mut self.buf)
            {
                Ok(n) => {
                    bad = 0;
                    n
                }
                Err(e) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        return Err(unprocessable("damaged Opus", e));
                    }
                    // Skipped as silence of its length, so what follows keeps its time; when
                    // even the length cannot be read, that of the last packet that decoded.
                    let n = opus_pure::packet::samples_48k(&p.data)
                        .unwrap_or(self.last_len)
                        .min(opus_pure::MAX_PACKET_SAMPLES);
                    self.buf[..n * self.channels].fill(0.0);
                    n
                }
            };
            if bad == 0 {
                self.last_len = n;
            }
            let first = self.cursor;
            self.cursor += n as u64;
            // The last page's granule position cuts the padding of the last frame.
            let end = match p.absgp_page() {
                g if last && g != u64::MAX => g.min(self.cursor),
                _ => self.cursor,
            };
            let from = self.pre_skip.max(first);
            if from >= end {
                continue;
            }
            let (a, b) = ((from - first) as usize, (end - first) as usize);
            let mut block = self.buf[a * self.channels..b * self.channels].to_vec();
            if self.gain != 1.0 {
                block.iter_mut().for_each(|s| *s *= self.gain);
            }
            return Ok(Some(block));
        }
    }
}

// ------------------------------------------------------------------------------ channels

/// Half power: −3 dB.
const HALF_POWER: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// The `[left, right]` weight of each of `count` channels when mixing down to stereo:
/// the front left and right go to their side at 1, the front centre to both at 1/√2
/// (−3 dB), every other left or right speaker (side, rear, wide, height) to its side at 1/√2,
/// any other centre speaker to both at 1/2, and the LFE nowhere. Channels without a position
/// are taken in the WAVE order (front left, front right, front centre, LFE, rear left, rear
/// right, ...). The weights are then scaled so that each side's add up to at most 1, so the
/// mix cannot clip; this is what ffmpeg does by default (5.1 gives each side 0.41 of its
/// front, 0.29 of the centre and 0.29 of its rear).
fn downmix_weights(layout: &Channels, count: usize) -> Vec<[f32; 2]> {
    let positions: Vec<Option<Position>> = match layout {
        Channels::Positioned(p) => (0..64)
            .map(|bit| 1u64 << bit)
            .filter(|bit| p.bits() & bit != 0)
            .map(Position::from_bits)
            .chain(std::iter::repeat(None))
            .take(count)
            .collect(),
        _ => (0..count)
            .map(|i| (i < 64).then(|| Position::from_bits(1 << i)).flatten())
            .collect(),
    };
    let mut weights: Vec<[f32; 2]> = positions.into_iter().map(position_weight).collect();
    let sum = |weights: &[[f32; 2]], side: usize| weights.iter().map(|w| w[side]).sum::<f32>();
    let scale = 1.0 / sum(&weights, 0).max(sum(&weights, 1)).max(1.0);
    for w in &mut weights {
        w[0] *= scale;
        w[1] *= scale;
    }
    weights
}

fn position_weight(pos: Option<Position>) -> [f32; 2] {
    let Some(pos) = pos else {
        return [0.5, 0.5];
    };
    let left = Position::REAR_LEFT
        | Position::FRONT_LEFT_CENTER
        | Position::SIDE_LEFT
        | Position::TOP_FRONT_LEFT
        | Position::TOP_REAR_LEFT
        | Position::TOP_SIDE_LEFT
        | Position::BOTTOM_FRONT_LEFT
        | Position::FRONT_LEFT_WIDE;
    let right = Position::REAR_RIGHT
        | Position::FRONT_RIGHT_CENTER
        | Position::SIDE_RIGHT
        | Position::TOP_FRONT_RIGHT
        | Position::TOP_REAR_RIGHT
        | Position::TOP_SIDE_RIGHT
        | Position::BOTTOM_FRONT_RIGHT
        | Position::FRONT_RIGHT_WIDE;
    if pos == Position::FRONT_LEFT {
        [1.0, 0.0]
    } else if pos == Position::FRONT_RIGHT {
        [0.0, 1.0]
    } else if pos == Position::FRONT_CENTER {
        [HALF_POWER, HALF_POWER]
    } else if pos.intersects(Position::LFE1 | Position::LFE2) {
        [0.0, 0.0]
    } else if left.contains(pos) {
        [HALF_POWER, 0.0]
    } else if right.contains(pos) {
        [0.0, HALF_POWER]
    } else {
        [0.5, 0.5]
    }
}

/// Mixes interleaved frames of `weights.len()` channels down to stereo.
fn downmix(block: &[f32], weights: &[[f32; 2]]) -> Vec<f32> {
    let mut out = Vec::with_capacity(block.len() / weights.len() * 2);
    for frame in block.chunks_exact(weights.len()) {
        let (mut l, mut r) = (0.0, 0.0);
        for (s, w) in frame.iter().zip(weights) {
            l += s * w[0];
            r += s * w[1];
        }
        out.push(l);
        out.push(r);
    }
    out
}

// ---------------------------------------------------------------------------- resampling

/// Streams interleaved audio through rubato's FFT resampler to 48 kHz, dropping the filter's
/// delay at the start and its tail past the input's length at the end.
struct Resampling {
    inner: Fft<f32>,
    channels: usize,
    ratio: f64,
    /// Input not yet given to the resampler.
    pending: Vec<f32>,
    /// Output frames still to drop: the filter's delay.
    skip: usize,
    in_frames: u64,
    out_frames: u64,
    /// Scratch for one call's output.
    out: Vec<f32>,
}

impl Resampling {
    fn new(rate: u32, channels: usize) -> Result<Resampling, EncodeError> {
        let inner = Fft::<f32>::new(
            rate as usize,
            OPUS_RATE as usize,
            RESAMPLE_CHUNK,
            channels,
            FixedSync::Input,
        )
        .map_err(|e| unprocessable(&format!("cannot resample from {rate} Hz"), e))?;
        Ok(Resampling {
            skip: inner.output_delay(),
            out: vec![0.0; inner.output_frames_max() * channels],
            inner,
            channels,
            ratio: f64::from(OPUS_RATE) / f64::from(rate),
            pending: Vec::new(),
            in_frames: 0,
            out_frames: 0,
        })
    }

    fn process(&mut self, block: &[f32]) -> Result<Vec<f32>, EncodeError> {
        self.in_frames += (block.len() / self.channels) as u64;
        self.pending.extend_from_slice(block);
        let mut out = Vec::new();
        while self.pending.len() / self.channels >= self.inner.input_frames_next() {
            self.step(None, &mut out)?;
        }
        Ok(out)
    }

    /// The rest of the input, padded with silence, then silence until the tail is out, so
    /// that `in_frames * ratio` frames came out in all.
    fn flush(&mut self) -> Result<Vec<f32>, EncodeError> {
        let mut out = Vec::new();
        let wanted = (self.in_frames as f64 * self.ratio).round() as u64;
        while self.out_frames < wanted {
            let have = self.pending.len() / self.channels;
            self.step(Some(have), &mut out)?;
        }
        let excess = (self.out_frames - wanted) as usize;
        out.truncate(out.len() - excess * self.channels);
        self.out_frames -= excess as u64;
        Ok(out)
    }

    /// One call to the resampler; `partial` is the number of real frames in a last, short
    /// chunk.
    fn step(&mut self, partial: Option<usize>, out: &mut Vec<f32>) -> Result<(), EncodeError> {
        let ch = self.channels;
        let need = self.inner.input_frames_next();
        if partial.is_some() {
            self.pending.resize(need * ch, 0.0);
        }
        let input = InterleavedSlice::new(&self.pending[..need * ch], ch, need)
            .map_err(|e| unprocessable("resampling failed", e))?;
        let room = self.out.len() / ch;
        let mut output = InterleavedSlice::new_mut(&mut self.out, ch, room)
            .map_err(|e| unprocessable("resampling failed", e))?;
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let (_, produced) = self
            .inner
            .process_into_buffer(&input, &mut output, Some(&indexing))
            .map_err(|e| unprocessable("resampling failed", e))?;
        self.pending.drain(..need * ch);
        let dropped = self.skip.min(produced);
        self.skip -= dropped;
        out.extend_from_slice(&self.out[dropped * ch..produced * ch]);
        self.out_frames += (produced - dropped) as u64;
        Ok(())
    }
}

// ------------------------------------------------------------------------------ encoding

/// libopus and an Ogg Opus stream (RFC 7845) around it.
struct OggOpus<W: Write> {
    writer: PacketWriter<'static, W>,
    encoder: opus::Encoder,
    channels: usize,
    pre_skip: u64,
    /// 48 kHz samples per channel given so far, not counting the padding.
    input: u64,
    /// Samples not yet a whole frame.
    pending: Vec<f32>,
    /// Packets encoded so far.
    packets: u64,
    /// The last packet, held back so that the final one can end the stream.
    held: Option<Vec<u8>>,
    packet: Vec<u8>,
}

impl<W: Write> OggOpus<W> {
    /// Sets up the encoder and writes `OpusHead` and `OpusTags`, each on its own page.
    fn new(out: W, channels: usize, input_rate: u32) -> Result<Self, EncodeError> {
        let failed = |e: opus::Error| unprocessable("cannot set up the Opus encoder", e);
        let layout = if channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        };
        let mut encoder =
            opus::Encoder::new(OPUS_RATE, layout, opus::Application::Audio).map_err(failed)?;
        encoder
            .set_bitrate(opus::Bitrate::Bits(BITRATE))
            .map_err(failed)?;
        encoder.set_vbr(true).map_err(failed)?;
        let lookahead = encoder.get_lookahead().map_err(failed)?;
        let pre_skip = u16::try_from(lookahead)
            .map_err(|e| unprocessable(&format!("unexpected encoder lookahead {lookahead}"), e))?;

        let mut head = b"OpusHead".to_vec();
        head.push(1); // version
        head.push(channels as u8);
        head.extend(pre_skip.to_le_bytes());
        head.extend(input_rate.to_le_bytes());
        head.extend(0i16.to_le_bytes()); // output gain
        head.push(0); // mapping family 0: mono or stereo
        let mut tags = b"OpusTags".to_vec();
        tags.extend((VENDOR.len() as u32).to_le_bytes());
        tags.extend(VENDOR.as_bytes());
        tags.extend(0u32.to_le_bytes()); // no comments

        let mut writer = PacketWriter::new(out);
        for packet in [head, tags] {
            writer
                .write_packet(packet, SERIAL, PacketWriteEndInfo::EndPage, 0)
                .map_err(write_failed)?;
        }
        Ok(OggOpus {
            writer,
            encoder,
            channels,
            pre_skip: u64::from(pre_skip),
            input: 0,
            pending: Vec::new(),
            packets: 0,
            held: None,
            packet: vec![0; MAX_PACKET],
        })
    }

    /// Encodes the whole frames of `samples` (interleaved, 48 kHz), keeping the rest.
    fn push(&mut self, samples: &[f32], cancel: &AtomicBool) -> Result<(), EncodeError> {
        self.input += (samples.len() / self.channels) as u64;
        self.encode_frames(samples, cancel)
    }

    fn encode_frames(&mut self, samples: &[f32], cancel: &AtomicBool) -> Result<(), EncodeError> {
        self.pending.extend_from_slice(samples);
        let frame = FRAME * self.channels;
        let whole = self.pending.len() / frame * frame;
        let mut at = 0;
        while at < whole {
            check(cancel)?;
            let n = self
                .encoder
                .encode_float(&self.pending[at..at + frame], &mut self.packet)
                .map_err(|e| unprocessable("Opus encoding failed", e))?;
            self.emit(self.packet[..n].to_vec())?;
            at += frame;
        }
        self.pending.drain(..whole);
        Ok(())
    }

    /// Writes the packet before this one, ending a page every [`PACKETS_PER_PAGE`].
    fn emit(&mut self, packet: Vec<u8>) -> Result<(), EncodeError> {
        if let Some(prev) = self.held.replace(packet) {
            // The granule position of a page counts every sample its packets decode to,
            // the pre-skip included (RFC 7845 §4).
            let granule = self.packets * FRAME as u64;
            let end = if self.packets.is_multiple_of(PACKETS_PER_PAGE) {
                PacketWriteEndInfo::EndPage
            } else {
                PacketWriteEndInfo::NormalPacket
            };
            self.writer
                .write_packet(prev, SERIAL, end, granule)
                .map_err(write_failed)?;
        }
        self.packets += 1;
        Ok(())
    }

    /// Pads with silence until the decoder will have put out the pre-skip and all of the
    /// input, writes the last packet with a granule position that cuts the padding off
    /// (RFC 7845 §4.4), and flushes.
    fn finish(mut self, cancel: &AtomicBool) -> Result<(), EncodeError> {
        let end = self.pre_skip + self.input;
        while self.packets * (FRAME as u64) < end {
            let short = FRAME - self.pending.len() / self.channels;
            self.encode_frames(&vec![0.0; short * self.channels], cancel)?;
        }
        let last = self
            .held
            .take()
            .expect("at least one packet: the pre-skip alone needs one");
        self.writer
            .write_packet(last, SERIAL, PacketWriteEndInfo::EndStream, end)
            .map_err(write_failed)?;
        self.writer.inner_mut().flush().map_err(write_failed)
    }
}

fn write_failed(e: io::Error) -> EncodeError {
    EncodeError::Io(format!("cannot write: {e}"))
}

#[cfg(test)]
mod tests;
