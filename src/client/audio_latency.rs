//! Keeps queued playback audio close to real time.
//!
//! The queue is drained at the output device's rate and filled at the peer's capture rate, so any
//! backlog a network stall delivers in one burst, and any surplus that clock drift adds, would stay
//! queued for the rest of the session and leave audio behind the video.

/// Cushion an output waits to rebuild after it ran dry, so it does not stutter on every packet.
pub(super) const RESUME_MS: usize = 40;
const MEASURE_WINDOW_MS: usize = 1_000;
const TARGET_TROUGH_MS: usize = 30;
const TRIM_TROUGH_MS: usize = 50;
const RESYNC_ABOVE_MS: usize = 200;
const RESYNC_KEEP_MS: usize = 60;
const MAX_SPEEDUP_PERCENT: usize = 5;
const QUIET_PEAK: f32 = 0.001;
const MILLISECONDS_PER_SECOND: usize = 1_000;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: usize = 2;

/// Interleaved samples covering `ms` milliseconds, always whole frames.
pub(super) fn duration_samples(sample_rate: u32, channels: usize, ms: usize) -> usize {
    sample_rate as usize * ms / MILLISECONDS_PER_SECOND * channels
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
    target_trough: usize,
    trim_trough: usize,
    resync_above: usize,
    resync_keep: usize,
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
        Self {
            channels,
            measure_window: samples(MEASURE_WINDOW_MS),
            target_trough: samples(TARGET_TROUGH_MS),
            trim_trough: samples(TRIM_TROUGH_MS),
            resync_above: samples(RESYNC_ABOVE_MS),
            resync_keep: samples(RESYNC_KEEP_MS),
            resyncing: false,
            measured: 0,
            trough: usize::MAX,
            trim_budget: 0,
            compressed: Vec::new(),
        }
    }

    /// Decides how `chunk` joins a queue that holds `queued` samples right now.
    pub(super) fn plan(&mut self, queued: usize, chunk: &[f32]) -> LatencyPlan {
        let level = queued + chunk.len();
        let bursting = level > self.resync_keep + chunk.len() / 2;
        if level > self.resync_above || (self.resyncing && bursting) {
            // What a stall held back is already behind the video; resume from the newest audio.
            // The rest of the burst lands before playback drains the queue, so it is trimmed as
            // well until a chunk arrives in real time again.
            self.resyncing = true;
            self.restart_measurement();
            return LatencyPlan {
                keep_queued: Some(self.resync_keep.saturating_sub(chunk.len())),
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
        self.trim_budget = if self.trough > self.trim_trough {
            (self.trough - self.target_trough) / self.channels * self.channels
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
