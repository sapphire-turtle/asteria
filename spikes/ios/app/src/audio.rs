//! Audio playback for the spike: a state machine that decides what to do with each
//! [`Command`] ([`next_state`]), and an [`Engine`] that owns the cpal output stream on its
//! own thread and makes the stream follow the state.
//!
//! Everything here is platform-neutral. On iOS, the audio session and the lock screen
//! (`ios::audio_session`, `ios::now_playing`) turn system events into [`Command`]s and send
//! them through [`Engine::sender`].

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SampleFormat, SizedSample, StreamConfig};

use crate::report::SharedReport;

/// The rate every source produces frames at; output at another rate is converted.
pub const SOURCE_RATE: u32 = 48_000;

/// How often the engine reports its position while playing.
const PLAYING_TICK: Duration = Duration::from_secs(5);

/// What to play.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A 440/554/659 Hz arpeggio at volume 0.2, 0.5 s per note, without end.
    Tone,
    /// `opus_probe::SAMPLE`, decoded once with opus-pure and looped.
    OpusSample,
}

impl Source {
    /// The title shown on the lock screen and in Control Center.
    pub fn title(self) -> &'static str {
        match self {
            Source::Tone => "Tone (440/554/659 Hz)",
            Source::OpusSample => "Opus sample",
        }
    }
}

/// Something that asks playback to change: a button, a remote command or a system event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Start `Source` from its beginning, or keep playing it if it already plays.
    Play(Source),
    /// Pause at the person's request.
    Pause,
    /// Continue a paused source.
    Resume,
    /// Pause when playing, resume when paused.
    Toggle,
    /// Stop and forget the position.
    Stop,
    /// The system took the audio away (a call, Siri, an alarm).
    InterruptionBegan,
    /// The interruption is over; `should_resume` is the system's permission to continue.
    InterruptionEnded {
        /// Whether the system says playback may continue.
        should_resume: bool,
    },
    /// The route lost its output device (headphones unplugged, Bluetooth gone).
    OldDeviceUnavailable,
}

/// Where playback is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayState {
    /// Nothing to play.
    Stopped,
    /// Playing `Source`.
    Playing(Source),
    /// Paused in the middle of `source`.
    Paused {
        /// What was playing.
        source: Source,
        /// True when an interruption paused it, so its end may resume playback.
        by_interruption: bool,
    },
}

impl PlayState {
    /// The source that is playing or paused, if any.
    pub fn source(self) -> Option<Source> {
        match self {
            PlayState::Stopped => None,
            PlayState::Playing(source) | PlayState::Paused { source, .. } => Some(source),
        }
    }
}

/// The state after `cmd`. Only an interruption marks a pause `by_interruption`, and only
/// such a pause is resumed when the interruption ends with `should_resume`. An
/// interruption that ends without permission to resume leaves an ordinary pause.
pub fn next_state(state: PlayState, cmd: Command) -> PlayState {
    use PlayState::{Paused, Playing, Stopped};
    let user_pause = |source| Paused {
        source,
        by_interruption: false,
    };
    match (state, cmd) {
        (_, Command::Play(source)) => Playing(source),
        (_, Command::Stop) => Stopped,
        (Stopped, _) => Stopped,
        (Playing(source), Command::Pause | Command::Toggle | Command::OldDeviceUnavailable) => {
            user_pause(source)
        }
        (Paused { source, .. }, Command::Pause) => user_pause(source),
        (Paused { source, .. }, Command::Resume | Command::Toggle) => Playing(source),
        (Playing(source), Command::InterruptionBegan) => Paused {
            source,
            by_interruption: true,
        },
        (
            Paused {
                source,
                by_interruption: true,
            },
            Command::InterruptionEnded { should_resume },
        ) => {
            if should_resume {
                Playing(source)
            } else {
                user_pause(source)
            }
        }
        (state, _) => state,
    }
}

/// The arpeggio's notes, in Hz.
const TONE_NOTES: [f32; 3] = [440.0, 554.0, 659.0];
/// How long each note lasts, in frames at [`SOURCE_RATE`] (0.5 s).
const TONE_NOTE_FRAMES: u64 = SOURCE_RATE as u64 / 2;
/// The tone's peak amplitude.
const TONE_VOLUME: f32 = 0.2;

/// The tone source: an endless arpeggio, one mono sample per frame at [`SOURCE_RATE`].
#[derive(Debug, Default)]
pub struct ToneGenerator {
    frame: u64,
    /// Phase in cycles, carried across notes so the wave has no jumps.
    phase: f32,
}

impl ToneGenerator {
    /// The next mono sample.
    pub fn next_sample(&mut self) -> f32 {
        let note = (self.frame / TONE_NOTE_FRAMES) as usize % TONE_NOTES.len();
        let sample = TONE_VOLUME * (self.phase * std::f32::consts::TAU).sin();
        self.phase = (self.phase + TONE_NOTES[note] / SOURCE_RATE as f32).fract();
        self.frame += 1;
        sample
    }
}

/// Decoded audio to loop: interleaved samples at [`SOURCE_RATE`].
struct Pcm {
    samples: Vec<f32>,
    channels: usize,
}

/// Decode the bundled Opus sample with opus-pure, the fastest candidate on the device.
fn decode_opus_sample() -> Result<Pcm, String> {
    use opus_probe::{Candidate, SAMPLE, decode, read_ogg_opus};
    let ogg = read_ogg_opus(SAMPLE).map_err(|e| e.to_string())?;
    let samples = decode(Candidate::OpusPure, &ogg).map_err(|e| e.to_string())?;
    let channels = usize::from(ogg.channels);
    if samples.len() < channels || channels == 0 {
        return Err("the sample decoded to no audio".into());
    }
    Ok(Pcm { samples, channels })
}

/// A source producing stereo frames at [`SOURCE_RATE`].
enum Frames {
    Tone(ToneGenerator),
    Pcm { pcm: Arc<Pcm>, frame: usize },
}

impl Frames {
    fn next_frame(&mut self) -> [f32; 2] {
        match self {
            Frames::Tone(tone) => {
                let x = tone.next_sample();
                [x, x]
            }
            Frames::Pcm { pcm, frame } => {
                let start = *frame * pcm.channels;
                let out = if pcm.channels == 1 {
                    [pcm.samples[start]; 2]
                } else {
                    [pcm.samples[start], pcm.samples[start + 1]]
                };
                *frame = (*frame + 1) % (pcm.samples.len() / pcm.channels);
                out
            }
        }
    }
}

/// Converts [`SOURCE_RATE`] frames to the output rate by linear interpolation; at
/// [`SOURCE_RATE`] itself it passes frames through unchanged.
struct Resampler {
    frames: Frames,
    /// Source frames consumed per output frame.
    step: f64,
    /// Position between `current` and `next`, in `[0, 1)`.
    pos: f64,
    current: [f32; 2],
    next: [f32; 2],
}

impl Resampler {
    fn new(mut frames: Frames, output_rate: u32) -> Self {
        let current = frames.next_frame();
        let next = frames.next_frame();
        Self {
            frames,
            step: f64::from(SOURCE_RATE) / f64::from(output_rate),
            pos: 0.0,
            current,
            next,
        }
    }

    fn next_frame(&mut self) -> [f32; 2] {
        let t = self.pos as f32;
        let out = [0, 1].map(|c| self.current[c] + (self.next[c] - self.current[c]) * t);
        self.pos += self.step;
        while self.pos >= 1.0 {
            self.pos -= 1.0;
            self.current = self.next;
            self.next = self.frames.next_frame();
        }
        out
    }
}

/// Write a stereo frame to an output frame of any channel count: mono gets the average,
/// channels past the second get silence.
fn write_frame<T: SizedSample + FromSample<f32>>(out: &mut [T], [l, r]: [f32; 2]) {
    if let [only] = out {
        *only = T::from_sample((l + r) / 2.0);
        return;
    }
    for (i, sample) in out.iter_mut().enumerate() {
        let x = match i {
            0 => l,
            1 => r,
            _ => 0.0,
        };
        *sample = T::from_sample(x);
    }
}

/// A running output stream for one source. Dropping it stops the sound.
struct Output {
    source: Source,
    stream: cpal::Stream,
    /// Output frames written since the stream was built.
    frames_played: Arc<AtomicU64>,
    rate: u32,
}

impl Output {
    fn elapsed_secs(&self) -> f64 {
        self.frames_played.load(Ordering::Relaxed) as f64 / f64::from(self.rate)
    }
}

/// Build a (paused) stream on the default output device that plays `frames`, recording the
/// output format, and later stream errors, in `report`.
fn build_output(source: Source, frames: Frames, report: &SharedReport) -> Result<Output, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or("no default output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.into();
    {
        let mut report = lock(report);
        report.set("audio.output_rate", config.sample_rate);
        report.set("audio.output_channels", config.channels);
        report.set("audio.output_format", format);
        report.set(
            "audio.resampled",
            if config.sample_rate == SOURCE_RATE {
                "no".to_owned()
            } else {
                format!("linear {SOURCE_RATE} -> {}", config.sample_rate)
            },
        );
    }
    let frames_played = Arc::new(AtomicU64::new(0));
    let resampler = Resampler::new(frames, config.sample_rate);
    let rate = config.sample_rate;
    let stream = match format {
        SampleFormat::F32 => {
            build_stream::<f32>(&device, config, resampler, &frames_played, report)
        }
        SampleFormat::I16 => {
            build_stream::<i16>(&device, config, resampler, &frames_played, report)
        }
        SampleFormat::U16 => {
            build_stream::<u16>(&device, config, resampler, &frames_played, report)
        }
        SampleFormat::I32 => {
            build_stream::<i32>(&device, config, resampler, &frames_played, report)
        }
        other => return Err(format!("unsupported output sample format {other}")),
    }?;
    Ok(Output {
        source,
        stream,
        frames_played,
        rate,
    })
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: StreamConfig,
    mut resampler: Resampler,
    frames_played: &Arc<AtomicU64>,
    report: &SharedReport,
) -> Result<cpal::Stream, String> {
    let channels = usize::from(config.channels);
    let frames_played = Arc::clone(frames_played);
    let report = Arc::clone(report);
    let errors = AtomicU64::new(0);
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in data.chunks_mut(channels) {
                    write_frame(frame, resampler.next_frame());
                }
                frames_played.fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
            },
            move |err: cpal::Error| {
                log::warn!("audio stream error: {err}");
                let count = errors.fetch_add(1, Ordering::Relaxed) + 1;
                lock(&report).set("audio.stream_error", event_value(count, &err.to_string()));
            },
            None,
        )
        .map_err(|e| e.to_string())
}

/// Lock the report even if a panicking thread poisoned it.
pub(crate) fn lock(report: &SharedReport) -> std::sync::MutexGuard<'_, crate::report::Report> {
    report.lock().unwrap_or_else(|e| e.into_inner())
}

/// A report value for an event that happened `count` times, the last time now:
/// `"<count> (last HH:MM:SS UTC[, detail])"`.
pub fn event_value(count: u64, detail: &str) -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        % 86_400;
    let time = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if detail.is_empty() {
        format!("{count} (last {time} UTC)")
    } else {
        format!("{count} (last {time} UTC, {detail})")
    }
}

/// The playback engine. Its thread owns the output stream (cpal streams cannot move
/// between threads on every platform); everything else talks to it with [`Command`]s.
pub struct Engine {
    tx: Sender<Command>,
    state: Arc<Mutex<PlayState>>,
}

impl Engine {
    /// Start the engine thread. `notify` runs on that thread with the state and the
    /// elapsed playing time in seconds after every command, and every 5 s while playing.
    /// Results and errors go to `report` under `audio.*`.
    pub fn start(
        report: SharedReport,
        notify: impl FnMut(PlayState, f64) + Send + 'static,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let state = Arc::new(Mutex::new(PlayState::Stopped));
        let shared = Arc::clone(&state);
        let spawned = thread::Builder::new()
            .name("audio-engine".into())
            .spawn(move || engine_thread(rx, shared, report, notify));
        if let Err(e) = spawned {
            log::error!("audio engine thread did not start: {e}");
        }
        Self { tx, state }
    }

    /// Ask for a change; the engine applies it shortly after.
    pub fn send(&self, cmd: Command) {
        // The thread only ends when every sender is gone, so this cannot fail while
        // `self` lives, unless the thread never started; then there is nothing to do.
        let _ = self.tx.send(cmd);
    }

    /// A sender for system callbacks (interruptions, remote commands).
    pub fn sender(&self) -> Sender<Command> {
        self.tx.clone()
    }

    /// The state after the last command the engine applied.
    pub fn state(&self) -> PlayState {
        *self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The engine thread: apply each command until every sender is dropped.
fn engine_thread(
    rx: Receiver<Command>,
    shared: Arc<Mutex<PlayState>>,
    report: SharedReport,
    mut notify: impl FnMut(PlayState, f64),
) {
    let mut state = PlayState::Stopped;
    let mut output: Option<Output> = None;
    let mut opus: Option<Arc<Pcm>> = None;
    loop {
        let cmd = if matches!(state, PlayState::Playing(_)) {
            match rx.recv_timeout(PLAYING_TICK) {
                Ok(cmd) => Some(cmd),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => break,
            }
        };
        if let Some(cmd) = cmd {
            let wanted = next_state(state, cmd);
            state = match apply(wanted, &mut output, &mut opus, &report) {
                Ok(()) => wanted,
                Err(e) => {
                    log::warn!("audio: {e}");
                    lock(&report).set("audio.error", &e);
                    output = None;
                    PlayState::Stopped
                }
            };
            lock(&report).set("audio.state", format!("{state:?}"));
            *shared.lock().unwrap_or_else(|e| e.into_inner()) = state;
        }
        notify(state, output.as_ref().map_or(0.0, Output::elapsed_secs));
    }
}

/// Make the output follow `state`: build a stream for a new source, play or pause it,
/// or drop it when stopped.
fn apply(
    state: PlayState,
    output: &mut Option<Output>,
    opus: &mut Option<Arc<Pcm>>,
    report: &SharedReport,
) -> Result<(), String> {
    match state {
        PlayState::Stopped => *output = None,
        PlayState::Paused { .. } => {
            if let Some(out) = output {
                out.stream.pause().map_err(|e| format!("pause: {e}"))?;
            }
        }
        PlayState::Playing(source) => {
            if output.as_ref().is_none_or(|out| out.source != source) {
                // Release the old stream before opening another.
                *output = None;
                let frames = match source {
                    Source::Tone => Frames::Tone(ToneGenerator::default()),
                    Source::OpusSample => Frames::Pcm {
                        pcm: opus_pcm(opus, report)?,
                        frame: 0,
                    },
                };
                *output = Some(build_output(source, frames, report)?);
            }
            if let Some(out) = output {
                out.stream.play().map_err(|e| format!("play: {e}"))?;
            }
        }
    }
    Ok(())
}

/// The decoded Opus sample, decoding it on first use.
fn opus_pcm(cache: &mut Option<Arc<Pcm>>, report: &SharedReport) -> Result<Arc<Pcm>, String> {
    if let Some(pcm) = cache {
        return Ok(Arc::clone(pcm));
    }
    let started = std::time::Instant::now();
    let pcm = Arc::new(decode_opus_sample().map_err(|e| format!("opus decode: {e}"))?);
    let secs = pcm.samples.len() as f64 / pcm.channels as f64 / f64::from(SOURCE_RATE);
    lock(report).set(
        "audio.opus_decode",
        format!(
            "{secs:.1} s of audio, {} ch, in {:.0} ms",
            pcm.channels,
            started.elapsed().as_secs_f64() * 1000.0
        ),
    );
    *cache = Some(Arc::clone(&pcm));
    Ok(pcm)
}

#[cfg(test)]
mod tests {
    use super::*;

    use Command::*;
    use PlayState::*;
    use Source::*;

    fn run(state: PlayState, cmds: &[Command]) -> PlayState {
        cmds.iter().fold(state, |s, &c| next_state(s, c))
    }

    fn paused(source: Source, by_interruption: bool) -> PlayState {
        PlayState::Paused {
            source,
            by_interruption,
        }
    }

    #[test]
    fn interruption_pauses_and_resumes_only_when_allowed() {
        let began = run(Playing(Tone), &[InterruptionBegan]);
        assert_eq!(began, paused(Tone, true));
        let refused = next_state(
            began,
            InterruptionEnded {
                should_resume: false,
            },
        );
        assert!(matches!(refused, PlayState::Paused { source: Tone, .. }));
        let allowed = run(
            Playing(Tone),
            &[
                InterruptionBegan,
                InterruptionEnded {
                    should_resume: true,
                },
            ],
        );
        assert_eq!(allowed, Playing(Tone));
    }

    #[test]
    fn refused_resume_is_not_revived_by_a_later_interruption_end() {
        let state = run(
            Playing(Tone),
            &[
                InterruptionBegan,
                InterruptionEnded {
                    should_resume: false,
                },
                InterruptionEnded {
                    should_resume: true,
                },
            ],
        );
        assert_eq!(state, paused(Tone, false));
    }

    #[test]
    fn user_pause_is_not_resumed_by_interruption_end() {
        let state = run(
            Playing(Tone),
            &[
                Pause,
                InterruptionEnded {
                    should_resume: true,
                },
            ],
        );
        assert_eq!(state, paused(Tone, false));
        // Pausing during an interruption makes it the person's pause.
        let state = run(
            Playing(OpusSample),
            &[
                InterruptionBegan,
                Pause,
                InterruptionEnded {
                    should_resume: true,
                },
            ],
        );
        assert_eq!(state, paused(OpusSample, false));
    }

    #[test]
    fn unplugging_pauses_and_toggle_resumes() {
        let state = next_state(Playing(OpusSample), OldDeviceUnavailable);
        assert_eq!(state, paused(OpusSample, false));
        assert_eq!(next_state(state, Toggle), Playing(OpusSample));
        assert_eq!(next_state(Stopped, OldDeviceUnavailable), Stopped);
        assert_eq!(
            next_state(paused(Tone, true), OldDeviceUnavailable),
            paused(Tone, true)
        );
    }

    #[test]
    fn toggle_and_stop_from_every_state() {
        let states = [
            Stopped,
            Playing(Tone),
            Playing(OpusSample),
            paused(Tone, false),
            paused(Tone, true),
            paused(OpusSample, false),
            paused(OpusSample, true),
        ];
        for state in states {
            assert_eq!(next_state(state, Stop), Stopped, "{state:?} + Stop");
            let toggled = next_state(state, Toggle);
            let expected = match state {
                Stopped => Stopped,
                Playing(source) => paused(source, false),
                PlayState::Paused { source, .. } => Playing(source),
            };
            assert_eq!(toggled, expected, "{state:?} + Toggle");
            let resumed = next_state(state, Resume);
            let expected = match state {
                Stopped => Stopped,
                Playing(source) | PlayState::Paused { source, .. } => Playing(source),
            };
            assert_eq!(resumed, expected, "{state:?} + Resume");
            let user_paused = next_state(state, Pause);
            let expected = match state {
                Stopped => Stopped,
                Playing(source) | PlayState::Paused { source, .. } => paused(source, false),
            };
            assert_eq!(user_paused, expected, "{state:?} + Pause");
            for source in [Tone, OpusSample] {
                assert_eq!(next_state(state, Play(source)), Playing(source));
            }
            // An interruption only changes a playing state.
            let interrupted = next_state(state, InterruptionBegan);
            let expected = match state {
                Playing(source) => paused(source, true),
                other => other,
            };
            assert_eq!(interrupted, expected, "{state:?} + InterruptionBegan");
        }
    }

    #[test]
    fn resampler_passes_source_rate_through_and_interpolates_between_frames() {
        let source: Vec<[f32; 2]> = {
            let mut tone = Frames::Tone(ToneGenerator::default());
            (0..8).map(|_| tone.next_frame()).collect()
        };
        let mut same = Resampler::new(Frames::Tone(ToneGenerator::default()), SOURCE_RATE);
        for frame in &source {
            assert_eq!(same.next_frame(), *frame);
        }
        let mut double = Resampler::new(Frames::Tone(ToneGenerator::default()), SOURCE_RATE * 2);
        for pair in source.windows(2) {
            assert_eq!(double.next_frame(), pair[0]);
            let mid = double.next_frame();
            assert!((mid[0] - (pair[0][0] + pair[1][0]) / 2.0).abs() < 1e-6);
        }
    }

    #[test]
    fn tone_is_bounded_and_not_silent() {
        let mut tone = ToneGenerator::default();
        let samples: Vec<f32> = (0..SOURCE_RATE).map(|_| tone.next_sample()).collect();
        assert!(samples.iter().all(|x| x.abs() <= 0.2));
        let rms = (samples.iter().map(|x| x * x).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(rms > 0.05, "rms {rms}");
    }
}
