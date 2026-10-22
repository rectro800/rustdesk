//! Keeps queued playback audio close to real time without starving it.
//!
//! The queue is drained at the output device's rate and filled at the peer's capture rate, so any
//! backlog a network stall delivers in one burst, and any surplus that clock drift adds, would stay
//! queued for the rest of the session and leave audio behind the video. Trimming it too eagerly
//! makes every small hiccup run the output dry instead, so the cushion kept against jitter grows
//! each time the output runs dry and shrinks back only after a long run without that.

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

const INITIAL_MARGIN_MS: usize = 40;
const MIN_MARGIN_MS: usize = 30;
const MAX_MARGIN_MS: usize = 200;
const MARGIN_STEP_MS: usize = 40;
const MARGIN_DECAY_MS: usize = 20;
const MARGIN_DECAY_AFTER_MS: usize = 5_000;
// A dry spell that ends in a resync was an outage, which no affordable cushion would cover.
const OUTAGE_GRACE_MS: usize = 1_000;
const RESUME_EXTRA_MS: usize = 20;
/// Cushion an output waits to rebuild after it ran dry, until the latency controller sets one.
pub(super) const RESUME_MS: usize = INITIAL_MARGIN_MS + RESUME_EXTRA_MS;
const TRIM_HYSTERESIS_MS: usize = 30;
const RESYNC_ABOVE_MARGIN_MS: usize = 300;
const RESYNC_KEEP_ABOVE_MARGIN_MS: usize = 40;
const MEASURE_WINDOW_MS: usize = 2_000;
const MAX_SPEEDUP_PERCENT: usize = 4;
pub(super) const QUIET_PEAK: f32 = 0.001;
const MILLISECONDS_PER_SECOND: usize = 1_000;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: usize = 2;

/// Interleaved samples covering `ms` milliseconds, always whole frames.
pub(super) fn duration_samples(sample_rate: u32, channels: usize, ms: usize) -> usize {
    sample_rate as usize * ms / MILLISECONDS_PER_SECOND * channels
}

/// Shared between the queue's writer and the output that drains it.
#[derive(Default)]
pub(super) struct PlaybackFeedback {
    underruns: AtomicUsize,
    resume_samples: AtomicUsize,
}

impl PlaybackFeedback {
    /// The output ran dry while it was playing something audible.
    pub(super) fn report_underrun(&self) {
        self.underruns.fetch_add(1, Ordering::Relaxed);
    }

    /// Samples to queue up before playing again after running dry.
    pub(super) fn resume_samples(&self) -> usize {
        self.resume_samples.load(Ordering::Relaxed)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChunkPlan {
    Keep,
    Drop,
    Compressed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct LatencyPlan {
    /// Drop the oldest queued samples until at most this many remain, then append.
    pub(super) keep_queued: Option<usize>,
    pub(super) chunk: ChunkPlan,
}

pub(super) struct PlaybackLatency {
    channels: usize,
    measure_window: usize,
    min_margin: usize,
    max_margin: usize,
    margin_step: usize,
    margin_decay: usize,
    margin_decay_after: usize,
    outage_grace: usize,
    resume_extra: usize,
    trim_hysteresis: usize,
    resync_above_margin: usize,
    resync_keep_above_margin: usize,
    margin: usize,
    raised: bool,
    clean: usize,
    seen_underruns: usize,
    feedback: Arc<PlaybackFeedback>,
    resyncing: bool,
    measured: usize,
    trough: usize,
    trim_budget: usize,
    compressed: Vec<f32>,
}

impl Default for PlaybackLatency {
    fn default() -> Self {
        Self::new(DEFAULT_SAMPLE_RATE, DEFAULT_CHANNELS)
    }
}

impl PlaybackLatency {
    pub(super) fn new(sample_rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let samples = |ms| duration_samples(sample_rate, channels, ms);
        let latency = Self {
            channels,
            measure_window: samples(MEASURE_WINDOW_MS),
            min_margin: samples(MIN_MARGIN_MS),
            max_margin: samples(MAX_MARGIN_MS),
            margin_step: samples(MARGIN_STEP_MS),
            margin_decay: samples(MARGIN_DECAY_MS),
            margin_decay_after: samples(MARGIN_DECAY_AFTER_MS),
            outage_grace: samples(OUTAGE_GRACE_MS),
            resume_extra: samples(RESUME_EXTRA_MS),
            trim_hysteresis: samples(TRIM_HYSTERESIS_MS),
            resync_above_margin: samples(RESYNC_ABOVE_MARGIN_MS),
            resync_keep_above_margin: samples(RESYNC_KEEP_ABOVE_MARGIN_MS),
            margin: samples(INITIAL_MARGIN_MS),
            raised: false,
            clean: 0,
            seen_underruns: 0,
            feedback: Default::default(),
            resyncing: false,
            measured: 0,
            trough: usize::MAX,
            trim_budget: 0,
            compressed: Vec::new(),
        };
        latency.publish_resume();
        latency
    }

    /// Hand this to the output that drains the queue.
    pub(super) fn feedback(&self) -> Arc<PlaybackFeedback> {
        self.feedback.clone()
    }

    /// Decides how `chunk` joins a queue that holds `queued` samples right now.
    pub(super) fn plan(&mut self, queued: usize, chunk: &[f32]) -> LatencyPlan {
        self.adapt_margin(chunk.len());
        let level = queued + chunk.len();
        let keep = self.margin + self.resync_keep_above_margin;
        let bursting = level > keep + chunk.len() / 2;
        if level > self.margin + self.resync_above_margin || (self.resyncing && bursting) {
            // What a stall held back is already behind the video; resume from the newest audio.
            // The rest of the burst lands before playback drains the queue, so it is trimmed as
            // well until a chunk arrives in real time again.
            if !self.resyncing && self.raised {
                self.margin = self.margin.saturating_sub(self.margin_step).max(self.min_margin);
                self.raised = false;
                self.publish_resume();
            }
            self.resyncing = true;
            self.restart_measurement();
            return LatencyPlan {
                keep_queued: Some(keep.saturating_sub(chunk.len())),
                chunk: ChunkPlan::Keep,
            };
        }
        self.resyncing = false;
        self.measure(queued, chunk.len());
        LatencyPlan {
            keep_queued: None,
            chunk: self.plan_chunk(chunk),
        }
    }

    /// The samples to append for a `plan` made from this `chunk`.
    pub(super) fn chunk<'a>(&'a self, chunk: &'a [f32], plan: &LatencyPlan) -> &'a [f32] {
        match plan.chunk {
            ChunkPlan::Keep => chunk,
            ChunkPlan::Drop => &[],
            ChunkPlan::Compressed => &self.compressed,
        }
    }

    fn adapt_margin(&mut self, appended: usize) {
        let underruns = self.feedback.underruns.load(Ordering::Relaxed);
        if underruns != self.seen_underruns {
            self.seen_underruns = underruns;
            self.raised = self.margin < self.max_margin;
            self.margin = (self.margin + self.margin_step).min(self.max_margin);
            self.clean = 0;
            // The trough measured before the hiccup would trim the cushion it just proved short.
            self.restart_measurement();
        } else {
            self.clean += appended;
            if self.clean >= self.outage_grace {
                self.raised = false;
            }
            if self.clean >= self.margin_decay_after {
                self.clean = 0;
                self.margin = self
                    .margin
                    .saturating_sub(self.margin_decay)
                    .max(self.min_margin);
            }
        }
        self.publish_resume();
    }

    fn publish_resume(&self) {
        self.feedback
            .resume_samples
            .store(self.margin + self.resume_extra, Ordering::Relaxed);
    }

    fn restart_measurement(&mut self) {
        self.measured = 0;
        self.trough = usize::MAX;
        self.trim_budget = 0;
    }

    fn measure(&mut self, queued: usize, appended: usize) {
        // The queue is lowest right before a chunk lands, so the lowest level over a window is
        // cushion that the jitter seen in that window never used.
        self.trough = self.trough.min(queued);
        self.measured += appended;
        if self.measured < self.measure_window {
            return;
        }
        if self.trough < self.margin / 2 {
            // Jitter used most of the cushion without running it dry; it is not too big yet.
            self.clean = 0;
        }
        self.trim_budget = if self.trough > self.margin + self.trim_hysteresis {
            (self.trough - self.margin) / self.channels * self.channels
        } else {
            0
        };
        self.measured = 0;
        self.trough = usize::MAX;
    }

    fn plan_chunk(&mut self, chunk: &[f32]) -> ChunkPlan {
        if self.trim_budget == 0 {
            return ChunkPlan::Keep;
        }
        if self.trim_budget >= chunk.len() && chunk.iter().all(|sample| sample.abs() < QUIET_PEAK)
        {
            self.trim_budget -= chunk.len();
            return ChunkPlan::Drop;
        }
        let frames = chunk.len() / self.channels;
        let removed = (frames * MAX_SPEEDUP_PERCENT / 100).min(self.trim_budget / self.channels);
        if removed == 0 || frames < removed + 2 {
            return ChunkPlan::Keep;
        }
        self.trim_budget -= removed * self.channels;
        self.compress(chunk, frames - removed);
        ChunkPlan::Compressed
    }

    /// Plays `chunk` slightly faster. Both end frames are kept, so consecutive chunks stay
    /// continuous without carrying resampler state between them.
    fn compress(&mut self, chunk: &[f32], frames: usize) {
        let channels = self.channels;
        let last = chunk.len() / channels - 1;
        let step = last as f64 / (frames - 1) as f64;
        self.compressed.clear();
        for frame in 0..frames {
            let position = frame as f64 * step;
            let index = (position as usize).min(last);
            let next = (index + 1).min(last);
            let weight = (position - index as f64) as f32;
            for channel in 0..channels {
                let from = chunk[index * channels + channel];
                let to = chunk[next * channels + channel];
                self.compressed.push(from + (to - from) * weight);
            }
        }
    }
}

#[cfg(test)]
#[path = "audio_latency_tests.rs"]
mod tests;
