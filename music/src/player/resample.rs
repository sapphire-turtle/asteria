//! Converting decoded audio to what the device takes: channels first, then the rate.

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{
    Async, FixedAsync, Indexing, Resampler, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

use super::engine::OutputConfig;

/// Input frames the resampler takes per call.
const CHUNK_FRAMES: usize = 1024;

/// Converts interleaved blocks from (rate, channels) to an [`OutputConfig`]: channels first
/// (mono is copied to both; more than two keeps the first two), then the rate with rubato.
/// The rate is left alone when it already matches.
pub struct Converter {
    from_channels: usize,
    to_channels: usize,
    resampler: Option<Resampling>,
}

struct Resampling {
    inner: Async<f32>,
    ratio: f64,
    /// Channel-converted input not yet given to the resampler.
    pending: Vec<f32>,
    /// Output frames still to drop: the filter's delay.
    skip: usize,
    in_frames: u64,
    out_frames: u64,
    /// Scratch for one call's output.
    out: Vec<f32>,
}

impl Converter {
    pub fn new(from_rate: u32, from_channels: u16, to: OutputConfig) -> Result<Converter, String> {
        if from_rate == 0 || from_channels == 0 || to.sample_rate == 0 || to.channels == 0 {
            return Err("the rate and the channel count must not be zero".into());
        }
        let to_channels = usize::from(to.channels);
        let resampler = if from_rate == to.sample_rate {
            None
        } else {
            let ratio = f64::from(to.sample_rate) / f64::from(from_rate);
            let params = SincInterpolationParameters {
                sinc_len: 128,
                f_cutoff: None,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Linear,
                window: WindowFunction::BlackmanHarris2,
            };
            let inner = Async::<f32>::new_sinc(
                ratio,
                1.0,
                &params,
                CHUNK_FRAMES,
                to_channels,
                FixedAsync::Input,
            )
            .map_err(|e| e.to_string())?;
            let out = vec![0.0; inner.output_frames_max() * to_channels];
            Some(Resampling {
                skip: inner.output_delay(),
                inner,
                ratio,
                pending: Vec::new(),
                in_frames: 0,
                out_frames: 0,
                out,
            })
        };
        Ok(Converter {
            from_channels: usize::from(from_channels),
            to_channels,
            resampler,
        })
    }

    /// The block with its channels mapped, then resampled. Samples may be held back until
    /// the next call (or [`Converter::flush`]).
    pub fn process(&mut self, block: &[f32]) -> Vec<f32> {
        let mapped = map_channels(block, self.from_channels, self.to_channels);
        match &mut self.resampler {
            None => mapped,
            Some(r) => {
                r.in_frames += (mapped.len() / self.to_channels) as u64;
                r.pending.extend_from_slice(&mapped);
                let mut out = Vec::new();
                r.run(self.to_channels, false, &mut out);
                out
            }
        }
    }

    /// What is left in the resampler at the end of the track.
    pub fn flush(&mut self) -> Vec<f32> {
        let mut out = Vec::new();
        if let Some(r) = &mut self.resampler {
            r.run(self.to_channels, true, &mut out);
        }
        out
    }
}

impl Resampling {
    /// Resamples whole chunks of `pending`; with `last`, also the rest of it and the
    /// resampler's tail, so that exactly `in_frames * ratio` frames came out in all.
    fn run(&mut self, ch: usize, last: bool, out: &mut Vec<f32>) {
        while self.pending.len() / ch >= self.inner.input_frames_next() {
            if !self.step(ch, None, out) {
                return;
            }
        }
        if !last {
            return;
        }
        let wanted = (self.in_frames as f64 * self.ratio).round() as u64;
        // The rest of the input, padded with silence, then silence until the tail is out.
        for _ in 0..16 {
            if self.out_frames >= wanted {
                break;
            }
            let have = self.pending.len() / ch;
            if !self.step(ch, Some(have), out) {
                break;
            }
        }
        // Whatever the filter produced past the length of the input is not music.
        let excess = (self.out_frames.saturating_sub(wanted)) as usize;
        out.truncate(out.len() - excess * ch);
        self.out_frames -= excess as u64;
    }

    /// One call to the resampler; `partial` is the number of real frames in a last,
    /// short chunk. Returns false when the resampler failed.
    fn step(&mut self, ch: usize, partial: Option<usize>, out: &mut Vec<f32>) -> bool {
        let need = self.inner.input_frames_next();
        if partial.is_some() {
            self.pending.resize(need * ch, 0.0);
        }
        let Ok(input) = InterleavedSlice::new(&self.pending[..need * ch], ch, need) else {
            return false;
        };
        let room = self.out.len() / ch;
        let Ok(mut output) = InterleavedSlice::new_mut(&mut self.out, ch, room) else {
            return false;
        };
        let indexing = Indexing {
            input_offset: 0,
            output_offset: 0,
            partial_len: partial,
            active_channels_mask: None,
        };
        let Ok((_, produced)) =
            self.inner
                .process_into_buffer(&input, &mut output, Some(&indexing))
        else {
            return false;
        };
        self.pending.drain(..need * ch);
        let dropped = self.skip.min(produced);
        self.skip -= dropped;
        out.extend_from_slice(&self.out[dropped * ch..produced * ch]);
        self.out_frames += (produced - dropped) as u64;
        true
    }
}

/// Maps interleaved frames from `from` channels to `to`.
fn map_channels(block: &[f32], from: usize, to: usize) -> Vec<f32> {
    if from == to {
        return block.to_vec();
    }
    let mut out = Vec::with_capacity(block.len() / from * to);
    for frame in block.chunks_exact(from) {
        if to == 1 {
            let n = from.min(2);
            out.push(frame[..n].iter().sum::<f32>() / n as f32);
        } else {
            for c in 0..to {
                out.push(frame[if from == 1 { 0 } else { c.min(from - 1) }]);
            }
        }
    }
    out
}
