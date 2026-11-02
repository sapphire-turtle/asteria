use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;

/// One 20 ms Opus frame at 48 kHz.
const FRAME: u64 = 960;

/// A 16-bit PCM WAV of `secs` seconds at `rate` with `channels` channels; channel `c` holds
/// `tone(c)` (a frequency in Hz, or 0 for silence) at half scale.
fn wav(path: &Path, rate: u32, channels: u16, frames: u32, tone: impl Fn(u16) -> f32) {
    let data_len = frames * u32::from(channels) * 2;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend(b"RIFF");
    b.extend((36 + data_len).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes()); // PCM
    b.extend(channels.to_le_bytes());
    b.extend(rate.to_le_bytes());
    b.extend((rate * u32::from(channels) * 2).to_le_bytes());
    b.extend((channels * 2).to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(data_len.to_le_bytes());
    for i in 0..frames {
        for c in 0..channels {
            let f = tone(c);
            let s = if f == 0.0 {
                0.0
            } else {
                0.5 * (2.0 * std::f32::consts::PI * f * i as f32 / rate as f32).sin()
            };
            b.extend(((s * f32::from(i16::MAX)) as i16).to_le_bytes());
        }
    }
    std::fs::write(path, b).unwrap();
}

/// What an Ogg Opus file holds.
#[derive(Debug)]
struct Decoded {
    channels: usize,
    pre_skip: u64,
    input_rate: u32,
    /// The granule position of the last page.
    final_granule: u64,
    /// The decoded 48 kHz frames after the pre-skip, trimmed to the final granule.
    samples: Vec<f32>,
    vendor: String,
    comments: u32,
}

impl Decoded {
    fn frames(&self) -> u64 {
        (self.samples.len() / self.channels) as u64
    }

    /// The root mean square of channel `c`.
    fn rms(&self, c: usize) -> f32 {
        let ch: Vec<f32> = self
            .samples
            .iter()
            .skip(c)
            .step_by(self.channels)
            .copied()
            .collect();
        (ch.iter().map(|s| s * s).sum::<f32>() / ch.len() as f32).sqrt()
    }
}

fn decode(path: &Path) -> Decoded {
    let mut r = ogg::PacketReader::new(std::fs::File::open(path).unwrap());
    let head = r.read_packet().unwrap().unwrap();
    assert!(
        head.first_in_stream() && head.last_in_page(),
        "OpusHead has its own page"
    );
    assert_eq!(head.absgp_page(), 0);
    let h = &head.data;
    assert_eq!(&h[..8], b"OpusHead");
    assert_eq!(h.len(), 19);
    assert_eq!(h[8], 1, "version 1");
    let channels = usize::from(h[9]);
    let pre_skip = u64::from(u16::from_le_bytes([h[10], h[11]]));
    let input_rate = u32::from_le_bytes(h[12..16].try_into().unwrap());
    assert_eq!(i16::from_le_bytes([h[16], h[17]]), 0, "no output gain");
    assert_eq!(h[18], 0, "mapping family 0");
    let tags = r.read_packet().unwrap().unwrap();
    assert!(tags.last_in_page(), "OpusTags has its own page");
    let t = &tags.data;
    assert_eq!(&t[..8], b"OpusTags");
    let vlen = u32::from_le_bytes(t[8..12].try_into().unwrap()) as usize;
    let vendor = String::from_utf8(t[12..12 + vlen].to_vec()).unwrap();
    let comments = u32::from_le_bytes(t[12 + vlen..16 + vlen].try_into().unwrap());
    let mut dec = opus_pure::OpusDecoder::new(48_000, channels).unwrap();
    let mut buf = vec![0f32; opus_pure::MAX_PACKET_SAMPLES * channels];
    let mut all = Vec::new();
    let mut final_granule = 0;
    let mut last_granule = 0;
    while let Some(p) = r.read_packet().unwrap() {
        let n = dec
            .decode(&p.data, opus_pure::MAX_PACKET_SAMPLES, &mut buf)
            .unwrap();
        all.extend_from_slice(&buf[..n * channels]);
        if p.last_in_page() {
            assert!(p.absgp_page() >= last_granule, "granules never go back");
            last_granule = p.absgp_page();
        }
        if p.last_in_stream() {
            final_granule = p.absgp_page();
        }
    }
    assert!(final_granule > 0, "the stream is ended");
    let decoded = (all.len() / channels) as u64;
    assert!(final_granule <= decoded, "{final_granule} > {decoded}");
    assert!(
        decoded - final_granule < FRAME,
        "only the last frame's padding is trimmed: {decoded} decoded, granule {final_granule}"
    );
    let samples = all[pre_skip as usize * channels..final_granule as usize * channels].to_vec();
    Decoded {
        channels,
        pre_skip,
        input_rate,
        final_granule,
        samples,
        vendor,
        comments,
    }
}

fn encode(src: &Path, dir: &Path) -> Result<PathBuf, EncodeError> {
    let dest = dir.join("out.ogg");
    encode_offline_audio(src, &dest, &AtomicBool::new(false)).map(|()| dest)
}

/// libopus's lookahead at 48 kHz with the audio application: 2.5 ms plus 4 ms.
const LOOKAHEAD: u64 = 312;

#[test]
fn a_stereo_44k_track_becomes_48k_opus() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 44_100, 2, 3 * 44_100, |c| [440.0, 660.0][c as usize]);
    let out = decode(&encode(&src, dir.path()).unwrap());
    assert_eq!(out.channels, 2);
    assert_eq!(out.input_rate, 44_100);
    assert_eq!(out.pre_skip, LOOKAHEAD);
    assert_eq!(out.final_granule, out.pre_skip + out.frames());
    assert!(
        out.frames().abs_diff(3 * 48_000) <= FRAME,
        "{} frames",
        out.frames()
    );
    // Half-scale sine waves have an RMS of about 0.35.
    for c in 0..2 {
        assert!((0.25..0.45).contains(&out.rms(c)), "{}", out.rms(c));
    }
    assert_eq!(out.vendor, "asteria");
    assert_eq!(out.comments, 0);
}

#[test]
fn a_mono_track_stays_mono() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 22_050, 1, 22_050, |_| 440.0);
    let out = decode(&encode(&src, dir.path()).unwrap());
    assert_eq!(out.channels, 1);
    assert_eq!(out.input_rate, 22_050);
    assert!(out.frames().abs_diff(48_000) <= FRAME, "{}", out.frames());
    assert!((0.25..0.45).contains(&out.rms(0)), "{}", out.rms(0));
}

#[test]
fn a_surround_track_is_downmixed_to_stereo() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    // 5.1 in WAV order: front left, front right, centre, LFE, back left, back right. Only
    // the front left and the centre sound.
    wav(&src, 48_000, 6, 48_000, |c| match c {
        0 => 440.0,
        2 => 660.0,
        _ => 0.0,
    });
    let out = decode(&encode(&src, dir.path()).unwrap());
    assert_eq!(out.channels, 2);
    assert_eq!(out.input_rate, 48_000);
    let (l, r) = (out.rms(0), out.rms(1));
    // The centre reaches both sides; the front left only the left.
    assert!(r > 0.05, "right {r}");
    assert!(l > r * 1.3, "left {l}, right {r}");
    // Nothing clips: every row of the matrix sums to at most one.
    assert!(out.samples.iter().all(|s| s.abs() <= 1.0));
}

#[test]
fn a_48k_track_is_not_resampled() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    // Not a whole number of frames, so the last one is padded.
    let frames = 48_000 + 123;
    wav(&src, 48_000, 2, frames, |_| 440.0);
    let out = decode(&encode(&src, dir.path()).unwrap());
    assert_eq!(out.frames(), u64::from(frames));
    assert_eq!(out.final_granule, LOOKAHEAD + u64::from(frames));
}

#[test]
fn an_ogg_opus_source_is_decoded_and_encoded_again() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 48_000, 2, 48_000, |_| 440.0);
    let first = dir.path().join("first.opus");
    encode_offline_audio(&src, &first, &AtomicBool::new(false)).unwrap();
    let again = dir.path().join("again");
    std::fs::create_dir(&again).unwrap();
    let out = decode(&encode(&first, &again).unwrap());
    assert_eq!(out.channels, 2);
    // The pre-skip and the padding of the first encode are not carried over.
    assert_eq!(out.frames(), 48_000);
    assert!((0.25..0.45).contains(&out.rms(0)), "{}", out.rms(0));
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../asteria-framework/tests/fixtures")
        .join(name)
}

#[test]
fn flac_and_mp3_are_encoded() {
    for (name, millis) in [("sine440.flac", 200), ("sine440.mp3", 235)] {
        let dir = tempfile::tempdir().unwrap();
        let out = decode(&encode(&fixture(name), dir.path()).unwrap());
        assert_eq!(out.channels, 1, "{name}");
        assert_eq!(out.input_rate, 44_100, "{name}");
        let expected = millis * 48;
        // MP3 may keep some of the encoder's padding.
        assert!(
            out.frames().abs_diff(expected) <= 2 * FRAME,
            "{name}: {} frames",
            out.frames()
        );
    }
}

#[test]
fn a_broken_file_is_unprocessable_and_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("x.flac");
    std::fs::write(
        &src,
        (0..4000u32)
            .map(|n| (n * 37 % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let dest = dir.path().join("out.ogg");
    let r = encode_offline_audio(&src, &dest, &AtomicBool::new(false));
    assert!(matches!(r, Err(EncodeError::Unprocessable(_))), "{r:?}");
    assert!(!dest.exists());
}

#[test]
fn a_missing_source_is_unprocessable() {
    let dir = tempfile::tempdir().unwrap();
    let r = encode(&dir.path().join("gone.flac"), dir.path());
    assert!(matches!(r, Err(EncodeError::Unprocessable(_))), "{r:?}");
}

#[test]
fn a_cancelled_encode_stops() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 44_100, 2, 60 * 44_100, |_| 440.0);
    let dest = dir.path().join("out.ogg");
    let cancel = AtomicBool::new(true);
    let start = Instant::now();
    let r = encode_offline_audio(&src, &dest, &cancel);
    assert_eq!(r, Err(EncodeError::Cancelled));
    assert!(start.elapsed() < Duration::from_secs(10));
    assert!(!dest.exists());
    // Set while it runs, it stops too.
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let setter = {
        let cancel = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            cancel.store(true, Ordering::SeqCst);
        })
    };
    let r = encode_offline_audio(&src, &dest, &cancel);
    setter.join().unwrap();
    assert_eq!(r, Err(EncodeError::Cancelled));
    assert!(!dest.exists());
}

#[test]
fn an_unwritable_destination_is_an_io_error() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 48_000, 1, 4_800, |_| 440.0);
    let r = encode_offline_audio(
        &src,
        &dir.path().join("no-such-dir/out.ogg"),
        &AtomicBool::new(false),
    );
    assert!(matches!(r, Err(EncodeError::Io(_))), "{r:?}");
}

/// `src` again as Ogg Opus, with audio packet `n` (counting from 0) replaced by `bad`.
fn with_bad_packet(src: &Path, dest: &Path, n: usize, bad: &[u8]) {
    let mut r = ogg::PacketReader::new(std::fs::File::open(src).unwrap());
    let mut w = ogg::PacketWriter::new(std::fs::File::create(dest).unwrap());
    let mut i = 0usize;
    while let Some(p) = r.read_packet().unwrap() {
        let end = if p.last_in_stream() {
            ogg::PacketWriteEndInfo::EndStream
        } else if p.last_in_page() {
            ogg::PacketWriteEndInfo::EndPage
        } else {
            ogg::PacketWriteEndInfo::NormalPacket
        };
        let (serial, granule) = (p.stream_serial(), p.absgp_page());
        // The two header packets come first.
        let data = if i == n + 2 { bad.to_vec() } else { p.data };
        w.write_packet(data, serial, end, granule).unwrap();
        i += 1;
    }
}

#[test]
fn an_undecodable_opus_packet_becomes_silence_of_its_length() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a.wav");
    wav(&src, 48_000, 1, 48_000, |_| 440.0);
    let good = dir.path().join("good.opus");
    encode_offline_audio(&src, &good, &AtomicBool::new(false)).unwrap();
    // A code 3 packet that says it holds no frames is invalid (RFC 6716 §3.2.5).
    let toc = {
        let mut r = ogg::PacketReader::new(std::fs::File::open(&good).unwrap());
        r.read_packet().unwrap();
        r.read_packet().unwrap();
        r.read_packet().unwrap().unwrap().data[0]
    };
    let bad = dir.path().join("bad.opus");
    with_bad_packet(&good, &bad, 25, &[toc | 0x03, 0x00]);
    let again = dir.path().join("again");
    std::fs::create_dir(&again).unwrap();
    let out = decode(&encode(&bad, &again).unwrap());
    // The skipped packet's 20 ms is kept as silence, so the length and timing are kept.
    assert_eq!(out.frames(), 48_000);
    let at = |ms: usize| {
        out.samples[ms * 48..(ms + 20) * 48]
            .iter()
            .map(|s| s.abs())
            .sum::<f32>()
    };
    assert!(
        at(25 * 20 - 312 / 48 + 5) < at(100),
        "a quiet gap where the packet was"
    );
}
