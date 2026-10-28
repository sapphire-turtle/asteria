//! Decoding a track: symphonia for the library's formats, ogg + opus-pure for Ogg Opus
//! (symphonia has no Opus decoder).

use std::collections::VecDeque;
use std::fmt;
use std::io::{self, Read, SeekFrom};

use ogg::{OggReadError, PacketReader};
use opus_pure::MAX_PACKET_SAMPLES;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{Time, TimeBase};

/// Decoded audio, block by block.
pub trait TrackDecoder: Send {
    fn sample_rate(&self) -> u32;
    fn channels(&self) -> u16;
    fn duration_ms(&self) -> Option<u64>;
    /// The next block of interleaved samples; `None` at the end.
    fn next_block(&mut self) -> Result<Option<Vec<f32>>, DecodeError>;
    /// Seeks to `ms` (clamped to the length); returns where it landed.
    fn seek(&mut self, ms: u64) -> Result<u64, DecodeError>;
}

#[derive(Debug)]
pub enum DecodeError {
    /// A format or codec feature that is not handled.
    Unsupported(String),
    /// The data is damaged.
    Corrupt(String),
    /// Reading the source failed (a stream failure is the inner `StreamError`).
    Io(io::Error),
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Unsupported(m) => write!(f, "unsupported: {m}"),
            DecodeError::Corrupt(m) => write!(f, "corrupt: {m}"),
            DecodeError::Io(e) => write!(f, "read failed: {e}"),
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

impl From<SymError> for DecodeError {
    fn from(e: SymError) -> Self {
        match e {
            SymError::IoError(e) => DecodeError::Io(e),
            SymError::Unsupported(m) | SymError::LimitError(m) => {
                DecodeError::Unsupported(m.into())
            }
            SymError::ResetRequired => DecodeError::Unsupported("the stream changed format".into()),
            e => DecodeError::Corrupt(e.to_string()),
        }
    }
}

impl From<OggReadError> for DecodeError {
    fn from(e: OggReadError) -> Self {
        match e {
            OggReadError::ReadError(e) => DecodeError::Io(e),
            e => DecodeError::Corrupt(e.to_string()),
        }
    }
}

/// Opens Ogg Opus (an `OggS` page whose first packet starts `OpusHead`) with ogg + opus-pure,
/// anything else with symphonia, using `ext` (the file's extension) as its hint.
pub fn open(
    mut source: Box<dyn MediaSource>,
    ext: Option<&str>,
) -> Result<Box<dyn TrackDecoder>, DecodeError> {
    if is_ogg_opus(&mut source)? {
        Ok(Box::new(OpusTrack::open(source)?))
    } else {
        Ok(Box::new(SymphoniaTrack::open(source, ext)?))
    }
}

fn is_ogg_opus(source: &mut Box<dyn MediaSource>) -> io::Result<bool> {
    let mut head = [0u8; 27 + 255 + 8];
    let mut got = 0;
    while got < head.len() {
        match source.read(&mut head[got..])? {
            0 => break,
            n => got += n,
        }
    }
    source.seek_start()?;
    let head = &head[..got];
    if head.len() < 27 || &head[..4] != b"OggS" {
        return Ok(false);
    }
    let body = 27 + usize::from(head[26]);
    Ok(head.get(body..body + 8) == Some(b"OpusHead"))
}

trait SeekStart {
    fn seek_start(&mut self) -> io::Result<()>;
}

impl SeekStart for Box<dyn MediaSource> {
    fn seek_start(&mut self) -> io::Result<()> {
        io::Seek::seek(self, SeekFrom::Start(0)).map(|_| ())
    }
}

// ---------------------------------------------------------------- symphonia

/// Decode errors in a row that are skipped before the track counts as damaged.
const MAX_BAD_PACKETS: u32 = 10;

struct SymphoniaTrack {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    channels: u16,
    duration_ms: Option<u64>,
    /// Frames to drop from the front of the next blocks (after a seek that landed early).
    skip_frames: usize,
    /// The frames of the track the container counts; a decoder that does not trim the
    /// encoder's padding gives more, and the rest is cut.
    total_frames: Option<u64>,
    /// Frames still to give out, when `total_frames` is known.
    remaining: Option<u64>,
    /// A block decoded to learn the rate and channels.
    peeked: Option<Vec<f32>>,
    ended: bool,
}

impl SymphoniaTrack {
    fn open(source: Box<dyn MediaSource>, ext: Option<&str>) -> Result<Self, DecodeError> {
        let mss = MediaSourceStream::new(source, Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = ext {
            hint.with_extension(ext);
        }
        let format = symphonia::default::get_probe().probe(
            &hint,
            mss,
            FormatOptions::default(),
            MetadataOptions::default(),
        )?;
        let track = format
            .default_track(TrackType::Audio)
            .ok_or_else(|| DecodeError::Unsupported("no audio track".into()))?;
        let params = track
            .codec_params
            .as_ref()
            .and_then(|p| p.audio())
            .ok_or_else(|| DecodeError::Unsupported("no audio codec parameters".into()))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())?;
        let rate = params.sample_rate;
        let channels = params.channels.as_ref().map(|c| c.count());
        let duration_ms = match (track.num_frames, rate) {
            (Some(n), Some(r)) if r > 0 => Some(n * 1000 / u64::from(r)),
            _ => track
                .duration
                .zip(track.time_base)
                .and_then(|(d, tb)| tb.calc_duration(d))
                .and_then(|t| u64::try_from(t.as_millis()).ok()),
        };
        let mut t = SymphoniaTrack {
            track_id: track.id,
            time_base: track.time_base,
            rate: rate.unwrap_or(0),
            channels: channels.map_or(0, |c| c as u16),
            duration_ms,
            total_frames: track.num_frames,
            remaining: track.num_frames,
            format,
            decoder,
            skip_frames: 0,
            peeked: None,
            ended: false,
        };
        if t.rate == 0 || t.channels == 0 {
            // Not in the headers: the first decoded block tells.
            t.peeked = t.decode_next()?;
            if t.rate == 0 || t.channels == 0 {
                return Err(DecodeError::Corrupt("no audio in the file".into()));
            }
        }
        Ok(t)
    }

    /// Decodes the next packet of the track that decodes; learns the rate and channels.
    fn decode_next(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        let mut bad = 0;
        loop {
            let packet = match self.format.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) => return Ok(None),
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(None);
                }
                Err(e) => return Err(e.into()),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(buf) => {
                    self.rate = buf.spec().rate();
                    self.channels = buf.spec().channels().count() as u16;
                    let mut samples = Vec::new();
                    buf.copy_to_vec_interleaved(&mut samples);
                    return Ok(Some(samples));
                }
                Err(SymError::DecodeError(m)) => {
                    bad += 1;
                    if bad >= MAX_BAD_PACKETS {
                        return Err(DecodeError::Corrupt(m.into()));
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Ticks of the track's time base as milliseconds.
    fn ticks_to_ms(&self, ticks: i64) -> f64 {
        match self.time_base {
            Some(tb) => {
                ticks as f64 * f64::from(tb.numer.get()) / f64::from(tb.denom.get()) * 1000.0
            }
            None => ticks as f64 * 1000.0 / f64::from(self.rate),
        }
    }
}

impl TrackDecoder for SymphoniaTrack {
    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn duration_ms(&self) -> Option<u64> {
        self.duration_ms
    }

    fn next_block(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        loop {
            if self.ended {
                return Ok(None);
            }
            let Some(mut block) = self
                .peeked
                .take()
                .map_or_else(|| self.decode_next(), |b| Ok(Some(b)))?
            else {
                self.ended = true;
                return Ok(None);
            };
            if self.skip_frames > 0 {
                let ch = usize::from(self.channels);
                let drop = self.skip_frames.min(block.len() / ch);
                self.skip_frames -= drop;
                block.drain(..drop * ch);
            }
            if let Some(left) = &mut self.remaining {
                let frames = (block.len() / usize::from(self.channels)) as u64;
                if frames >= *left {
                    block.truncate(*left as usize * usize::from(self.channels));
                    self.ended = true;
                }
                *left = left.saturating_sub(frames);
            }
            if !block.is_empty() {
                return Ok(Some(block));
            }
        }
    }

    fn seek(&mut self, ms: u64) -> Result<u64, DecodeError> {
        let ms = self.duration_ms.map_or(ms, |d| ms.min(d));
        self.peeked = None;
        self.skip_frames = 0;
        self.ended = false;
        self.remaining = self
            .total_frames
            .map(|n| n.saturating_sub(ms * u64::from(self.rate) / 1000));
        let time = Time::try_from_nanos_i128(i128::from(ms) * 1_000_000)
            .ok_or_else(|| DecodeError::Unsupported("seek position out of range".into()))?;
        match self.format.seek(
            SeekMode::Accurate,
            SeekTo::Time {
                time,
                track_id: Some(self.track_id),
            },
        ) {
            Ok(to) => {
                self.decoder.reset();
                let early_ms = self.ticks_to_ms(to.required_ts.get() - to.actual_ts.get());
                if early_ms > 0.0 {
                    self.skip_frames = (early_ms * f64::from(self.rate) / 1000.0).round() as usize;
                }
                Ok(ms)
            }
            // At or past the end.
            Err(SymError::SeekError(_)) => {
                self.ended = true;
                Ok(ms)
            }
            Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.ended = true;
                Ok(ms)
            }
            Err(e) => Err(e.into()),
        }
    }
}

// --------------------------------------------------------------------- opus

/// Opus always decodes at 48 kHz here, and granule positions count 48 kHz samples.
const OPUS_RATE: u32 = 48_000;
/// How far before the wanted position a seek starts decoding, so that the decoder has
/// settled by then.
const PRE_ROLL_GRANULES: u64 = 80 * 48;

struct OpusTrack {
    reader: PacketReader<Box<dyn MediaSource>>,
    decoder: opus_pure::OpusDecoder,
    channels: usize,
    pre_skip: u64,
    /// Linear gain from the header's output gain.
    gain: f32,
    /// Granule position of the last page: the length, including the pre-skip.
    final_granule: Option<u64>,
    /// Packets already read, not yet decoded (those of the page a seek landed on).
    queued: VecDeque<Vec<u8>>,
    /// Granule position of the next frame to decode.
    cursor: u64,
    /// Frames before this granule position are dropped.
    start: u64,
    ended: bool,
}

impl OpusTrack {
    fn open(source: Box<dyn MediaSource>) -> Result<Self, DecodeError> {
        let mut reader = PacketReader::new(source);
        let head = reader
            .read_packet()?
            .ok_or_else(|| DecodeError::Corrupt("no packets in the stream".into()))?;
        let (channels, pre_skip, gain) = parse_head(&head.data)?;
        // OpusTags.
        reader
            .read_packet()?
            .ok_or_else(|| DecodeError::Corrupt("no OpusTags".into()))?;
        let final_granule = last_granule(&mut reader)?;
        let decoder = opus_pure::OpusDecoder::new(OPUS_RATE as i32, channels)
            .map_err(|e| DecodeError::Unsupported(e.to_string()))?;
        let mut t = OpusTrack {
            reader,
            decoder,
            channels,
            pre_skip,
            gain,
            final_granule,
            queued: VecDeque::new(),
            cursor: 0,
            start: pre_skip,
            ended: false,
        };
        t.restart()?;
        Ok(t)
    }

    /// Back to the first audio packet.
    fn restart(&mut self) -> Result<(), DecodeError> {
        self.reader.seek_bytes(SeekFrom::Start(0))?;
        for _ in 0..2 {
            self.reader.read_packet()?;
        }
        self.queued.clear();
        self.cursor = 0;
        Ok(())
    }
}

/// `(channels, pre_skip, gain)` of an `OpusHead`; only mapping family 0 is handled.
fn parse_head(data: &[u8]) -> Result<(usize, u64, f32), DecodeError> {
    if data.len() < 19 || &data[..8] != b"OpusHead" {
        return Err(DecodeError::Corrupt(
            "the first packet is not OpusHead".into(),
        ));
    }
    let channels = usize::from(data[9]);
    let pre_skip = u64::from(u16::from_le_bytes([data[10], data[11]]));
    let gain_q8 = i16::from_le_bytes([data[16], data[17]]);
    let family = data[18];
    if family != 0 || !(1..=2).contains(&channels) {
        return Err(DecodeError::Unsupported(format!(
            "Opus channel mapping family {family} with {channels} channels"
        )));
    }
    Ok((
        channels,
        pre_skip,
        10f32.powf(f32::from(gain_q8) / (256.0 * 20.0)),
    ))
}

/// The granule position of the last page, read from the tail of the file; the reader is
/// left at the start of the stream.
fn last_granule(
    reader: &mut PacketReader<Box<dyn MediaSource>>,
) -> Result<Option<u64>, DecodeError> {
    const TAIL: i64 = 64 * 1024;
    let src = reader.get_mut();
    let len = io::Seek::seek(src, SeekFrom::End(0))?;
    let from = len.saturating_sub(TAIL as u64);
    io::Seek::seek(src, SeekFrom::Start(from))?;
    let mut tail = Vec::new();
    src.take(TAIL as u64).read_to_end(&mut tail)?;
    let granule = tail
        .windows(14)
        .enumerate()
        .rev()
        .filter(|(_, w)| &w[..4] == b"OggS")
        .map(|(_, w)| u64::from_le_bytes(w[6..14].try_into().expect("8 bytes")))
        .find(|g| *g != u64::MAX);
    reader.seek_bytes(SeekFrom::Start(0))?;
    // Past the two header packets again.
    for _ in 0..2 {
        reader.read_packet()?;
    }
    Ok(granule)
}

impl TrackDecoder for OpusTrack {
    fn sample_rate(&self) -> u32 {
        OPUS_RATE
    }

    fn channels(&self) -> u16 {
        self.channels as u16
    }

    fn duration_ms(&self) -> Option<u64> {
        self.final_granule
            .map(|g| g.saturating_sub(self.pre_skip) * 1000 / u64::from(OPUS_RATE))
    }

    fn next_block(&mut self) -> Result<Option<Vec<f32>>, DecodeError> {
        let mut buf = vec![0f32; MAX_PACKET_SAMPLES * self.channels];
        loop {
            if self.ended || self.final_granule.is_some_and(|g| self.cursor >= g) {
                return Ok(None);
            }
            let data = match self.queued.pop_front() {
                Some(d) => d,
                None => match self.reader.read_packet()? {
                    Some(p) => p.data,
                    None => {
                        self.ended = true;
                        return Ok(None);
                    }
                },
            };
            let n = self
                .decoder
                .decode(&data, MAX_PACKET_SAMPLES, &mut buf)
                .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
            let first = self.cursor;
            self.cursor += n as u64;
            let end = self
                .final_granule
                .map_or(self.cursor, |g| g.min(self.cursor));
            let from = self.start.max(first);
            if from >= end {
                continue;
            }
            let (a, b) = ((from - first) as usize, (end - first) as usize);
            let mut block = buf[a * self.channels..b * self.channels].to_vec();
            if self.gain != 1.0 {
                block.iter_mut().for_each(|s| *s *= self.gain);
            }
            return Ok(Some(block));
        }
    }

    fn seek(&mut self, ms: u64) -> Result<u64, DecodeError> {
        let ms = self.duration_ms().map_or(ms, |d| ms.min(d));
        let target = self.pre_skip + ms * u64::from(OPUS_RATE) / 1000;
        self.ended = false;
        self.start = target;
        self.decoder
            .reset_state()
            .map_err(|e| DecodeError::Corrupt(e.to_string()))?;
        if self.final_granule.is_some_and(|g| target >= g) {
            self.queued.clear();
            self.ended = true;
            return Ok(ms);
        }
        let goal = target.saturating_sub(PRE_ROLL_GRANULES);
        if goal == 0 {
            self.restart()?;
            return Ok(ms);
        }
        if !self.reader.seek_absgp(None, goal)? {
            self.queued.clear();
            self.ended = true;
            return Ok(ms);
        }
        // The packets of the page it landed on; the page's granule position is where its
        // last packet ends, which gives where its first one starts.
        self.queued.clear();
        let mut total = 0u64;
        let mut page_end = None;
        while let Some(p) = self.reader.read_packet()? {
            total += opus_pure::packet::samples_48k(&p.data)
                .map_err(|e| DecodeError::Corrupt(e.to_string()))? as u64;
            let last = p.last_in_page();
            let granule = p.absgp_page();
            self.queued.push_back(p.data);
            if last {
                page_end = Some(granule);
                break;
            }
        }
        match page_end {
            Some(g) if g != u64::MAX => self.cursor = g.saturating_sub(total),
            _ => {
                self.queued.clear();
                self.ended = true;
            }
        }
        Ok(ms)
    }
}
