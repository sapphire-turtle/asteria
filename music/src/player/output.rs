//! The sound device: cpal's default output, fed through a ring buffer.
//!
//! Numbers: the ring holds [`RING_MS`] (200 ms) of audio. The player thread pumps every
//! 10 ms and the feed hands over ~10 ms chunks, so the ring sits near full while playing and
//! survives a 190 ms stall of the player thread (a slow disk or a busy machine) before the
//! device runs dry. Latency of pause/seek/volume is not 200 ms: pause is a flag the
//! callback sees at once, and `clear` makes the callback drop what is buffered.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, Sample, SampleFormat, SizedSample, StreamConfig};
use rtrb::{Consumer, Producer, RingBuffer};

use super::engine::{AudioOutput, OutputConfig, OutputFactory};

/// How much audio the ring holds, in ms.
pub const RING_MS: u32 = 200;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Shared {
    /// Frames the callback has handed to the device (silence excluded).
    played: AtomicU64,
    /// Samples written before the last `clear`: the callback drops them unread.
    flush_to: AtomicU64,
    failed: Mutex<Option<String>>,
}

/// The engine's side of the ring.
pub struct RingWriter {
    producer: Producer<f32>,
    shared: Arc<Shared>,
    channels: usize,
    /// Samples ever written.
    written: u64,
}

/// The device callback's side of the ring.
pub struct RingReader {
    consumer: Consumer<f32>,
    shared: Arc<Shared>,
    channels: usize,
    /// Samples ever read or dropped.
    read: u64,
}

/// A ring of `RING_MS` of audio at `rate` and `channels`, split in its two sides.
pub fn ring(rate: u32, channels: u16) -> (RingWriter, RingReader) {
    let channels = usize::from(channels.max(1));
    let frames = (u64::from(rate) * u64::from(RING_MS) / 1000).max(1) as usize;
    let (producer, consumer) = RingBuffer::new(frames * channels);
    let shared = Arc::new(Shared::default());
    (
        RingWriter {
            producer,
            shared: shared.clone(),
            channels,
            written: 0,
        },
        RingReader {
            consumer,
            shared,
            channels,
            read: 0,
        },
    )
}

impl RingWriter {
    /// Free room, in frames.
    pub fn writable(&self) -> usize {
        self.producer.slots() / self.channels
    }

    /// Writes whole frames of `samples`, as many as fit.
    pub fn write(&mut self, samples: &[f32]) {
        let room = self.producer.slots();
        let n = samples.len().min(room) / self.channels * self.channels;
        if n == 0 {
            return;
        }
        if let Ok(mut chunk) = self.producer.write_chunk_uninit(n) {
            let (a, b) = chunk.as_mut_slices();
            let (head, tail) = samples[..n].split_at(a.len());
            for (d, s) in a.iter_mut().zip(head) {
                d.write(*s);
            }
            for (d, s) in b.iter_mut().zip(tail) {
                d.write(*s);
            }
            // SAFETY: every one of the `n` slots was written above.
            unsafe { chunk.commit_all() };
            self.written += n as u64;
        }
    }

    /// Frames the callback has played.
    pub fn played(&self) -> u64 {
        self.shared.played.load(Ordering::Acquire)
    }

    /// Drops what is buffered: the callback skips everything written so far.
    pub fn clear(&mut self) {
        self.shared.flush_to.store(self.written, Ordering::Release);
    }
}

impl RingReader {
    /// Fills `out` from the ring, converting from f32. Silence when paused or dry.
    pub fn fill<T: Sample + FromSample<f32>>(&mut self, out: &mut [T], playing: bool) {
        let flush_to = self.shared.flush_to.load(Ordering::Acquire);
        if self.read < flush_to {
            let n = ((flush_to - self.read) as usize).min(self.consumer.slots());
            if let Ok(chunk) = self.consumer.read_chunk(n) {
                chunk.commit_all();
                self.read += n as u64;
            }
        }
        let mut done = 0;
        if playing && self.read >= flush_to {
            let want = out.len() / self.channels * self.channels;
            let n = want.min(self.consumer.slots() / self.channels * self.channels);
            if n > 0
                && let Ok(chunk) = self.consumer.read_chunk(n)
            {
                let (a, b) = chunk.as_slices();
                for (d, s) in out.iter_mut().zip(a.iter().chain(b)) {
                    *d = T::from_sample(s.clamp(-1.0, 1.0));
                }
                chunk.commit_all();
                self.read += n as u64;
                self.shared
                    .played
                    .fetch_add((n / self.channels) as u64, Ordering::AcqRel);
                done = n;
            }
        }
        for d in &mut out[done..] {
            *d = T::EQUILIBRIUM;
        }
    }
}

/// cpal's default output device, at its default config, as f32. Samples go through an rtrb
/// ring of about 200 ms; the callback writes silence when it runs dry or while paused.
pub struct CpalOutputs;

impl OutputFactory for CpalOutputs {
    fn open(&mut self) -> Result<Box<dyn AudioOutput>, String> {
        // The stream is not `Send` everywhere, so a thread of its own builds and holds it.
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = std::thread::Builder::new()
            .name("asteria-music-stream".into())
            .spawn(move || match build() {
                Ok((stream, out)) => {
                    let _ = ready_tx.send(Ok(out));
                    // Until the output is dropped.
                    let _ = stop_rx.recv();
                    drop(stream);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv() {
            Ok(Ok(mut out)) => {
                out.stop = Some(stop_tx);
                out.thread = Some(thread);
                Ok(Box::new(out))
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = thread.join();
                Err("the audio thread died".into())
            }
        }
    }
}

fn build() -> Result<(cpal::Stream, CpalOutput), String> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| "no audio output device".to_string())?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config: StreamConfig = supported.config();
    let (writer, reader) = ring(config.sample_rate, config.channels);
    let paused = Arc::new(AtomicBool::new(true));
    let shared = writer.shared.clone();
    let stream = match supported.sample_format() {
        SampleFormat::F32 => stream::<f32>(&device, config, reader, &paused, &shared),
        SampleFormat::I16 => stream::<i16>(&device, config, reader, &paused, &shared),
        SampleFormat::U16 => stream::<u16>(&device, config, reader, &paused, &shared),
        SampleFormat::I32 => stream::<i32>(&device, config, reader, &paused, &shared),
        SampleFormat::I8 => stream::<i8>(&device, config, reader, &paused, &shared),
        SampleFormat::U8 => stream::<u8>(&device, config, reader, &paused, &shared),
        other => Err(format!(
            "the device's sample format {other} is not supported"
        )),
    }?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((
        stream,
        CpalOutput {
            writer,
            config: OutputConfig {
                sample_rate: config.sample_rate,
                channels: config.channels,
            },
            paused,
            stop: None,
            thread: None,
        },
    ))
}

fn stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut reader: RingReader,
    paused: &Arc<AtomicBool>,
    shared: &Arc<Shared>,
) -> Result<cpal::Stream, String> {
    let paused = paused.clone();
    let shared = shared.clone();
    device
        .build_output_stream::<T, _, _>(
            config,
            move |out, _| reader.fill(out, !paused.load(Ordering::Acquire)),
            move |e| {
                // A glitch or a reroute is not the end of the device.
                if matches!(
                    e.kind(),
                    ErrorKind::Xrun | ErrorKind::RealtimeDenied | ErrorKind::DeviceChanged
                ) {
                    return;
                }
                *lock(&shared.failed) = Some(e.to_string());
            },
            None,
        )
        .map_err(|e| e.to_string())
}

struct CpalOutput {
    writer: RingWriter,
    config: OutputConfig,
    paused: Arc<AtomicBool>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl AudioOutput for CpalOutput {
    fn config(&self) -> OutputConfig {
        self.config
    }
    fn writable(&self) -> usize {
        self.writer.writable()
    }
    fn write(&mut self, samples: &[f32]) {
        self.writer.write(samples);
    }
    fn played(&self) -> u64 {
        self.writer.played()
    }
    fn set_paused(&mut self, paused: bool) {
        self.paused.store(paused, Ordering::Release);
    }
    fn clear(&mut self) {
        self.writer.clear();
    }
    fn failed(&self) -> Option<String> {
        lock(&self.writer.shared.failed).clone()
    }
}

impl Drop for CpalOutput {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ring_counts_only_real_frames_as_played() {
        let (mut w, mut r) = ring(48_000, 2);
        w.write(&[0.5; 8]); // 4 frames
        let mut out = [1.0f32; 12]; // 6 frames wanted
        r.fill(&mut out, true);
        assert_eq!(&out[..8], &[0.5; 8]);
        assert_eq!(&out[8..], &[0.0; 4], "the rest is silence");
        assert_eq!(w.played(), 4);
        r.fill(&mut out, true);
        assert_eq!(w.played(), 4, "a dry callback plays nothing");
        w.write(&[0.25; 4]);
        r.fill(&mut out, false);
        assert_eq!(out, [0.0; 12], "paused: silence");
        assert_eq!(w.played(), 4);
        r.fill(&mut out, true);
        assert_eq!(w.played(), 6);
    }

    #[test]
    fn clear_drops_what_is_buffered_but_not_what_comes_after() {
        let (mut w, mut r) = ring(1000, 1);
        let room = w.writable();
        w.write(&[1.0; 100]);
        assert_eq!(w.writable(), room - 100);
        w.clear();
        w.write(&[0.5; 10]);
        let mut out = [0.0f32; 20];
        r.fill(&mut out, true);
        assert_eq!(&out[..10], &[0.5; 10]);
        assert_eq!(&out[10..], &[0.0; 10]);
        assert_eq!(w.played(), 10);
        assert_eq!(w.writable(), room);
    }

    #[test]
    fn i16_devices_get_converted_samples() {
        let (mut w, mut r) = ring(1000, 1);
        w.write(&[1.0, -1.0, 0.0, 0.5]);
        let mut out = [7i16; 5];
        r.fill(&mut out, true);
        assert_eq!(out[0], i16::MAX);
        assert!(out[1] <= -i16::MAX);
        assert_eq!(out[2], 0);
        assert!((out[3] - 16384).abs() <= 1);
        assert_eq!(out[4], 0);
        let mut u = [0u16; 1];
        w.write(&[0.0]);
        r.fill(&mut u, true);
        assert_eq!(u[0], 32768);
    }
}
