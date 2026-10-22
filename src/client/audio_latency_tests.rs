use super::{duration_samples, PlaybackFeedback, PlaybackLatency};
use std::{collections::VecDeque, sync::Arc};

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const PACKET_MS: usize = 10;
const PACKET_FRAMES: usize = SAMPLE_RATE as usize * PACKET_MS / 1_000;
const TONE_STEP: f32 = std::f32::consts::TAU * 441.0 / SAMPLE_RATE as f32;

/// A queue drained one millisecond at a time, as an output device would.
struct Playback {
    latency: PlaybackLatency,
    feedback: Arc<PlaybackFeedback>,
    queue: VecDeque<f32>,
    sent: Vec<f32>,
    played: Vec<f32>,
    starved: bool,
    dropouts: usize,
    resyncs: usize,
    phase: usize,
}

impl Playback {
    fn new(prefill_ms: usize) -> Self {
        let latency = PlaybackLatency::new(SAMPLE_RATE, CHANNELS);
        let mut playback = Self {
            feedback: latency.feedback(),
            latency,
            queue: VecDeque::new(),
            sent: Vec::new(),
            played: Vec::new(),
            starved: false,
            dropouts: 0,
            resyncs: 0,
            phase: 0,
        };
        let prefill = playback.packet(SAMPLE_RATE as usize * prefill_ms / 1_000);
        playback.queue.extend(&prefill);
        playback
    }

    /// A tone stays well above the quiet threshold, so only time compression may trim it.
    fn packet(&mut self, frames: usize) -> Vec<f32> {
        let mut samples = Vec::with_capacity(frames * CHANNELS);
        for _ in 0..frames {
            self.phase += 1;
            let sample = (self.phase as f32 * TONE_STEP).sin() * 0.5;
            samples.extend_from_slice(&[sample, -sample]);
        }
        self.sent.extend_from_slice(&samples);
        samples
    }

    fn deliver(&mut self, packet: &[f32]) {
        let plan = self.latency.plan(self.queue.len(), packet);
        if let Some(keep) = plan.keep_queued {
            let dropped = self.queue.len().saturating_sub(keep);
            self.queue.drain(..dropped);
            self.resyncs += 1;
        }
        self.queue.extend(self.latency.chunk(packet, &plan));
    }

    fn play_millisecond(&mut self) {
        let wanted = duration_samples(SAMPLE_RATE, CHANNELS, 1);
        if self.starved && self.queue.len() < self.feedback.resume_samples().max(wanted) {
            return;
        }
        let available = wanted.min(self.queue.len());
        self.played.extend(self.queue.drain(..available));
        let starved = available < wanted;
        if starved && !self.starved {
            self.dropouts += 1;
            self.feedback.report_underrun();
        }
        self.starved = starved;
    }

    /// Runs `ms` of real time with a packet of `frames` sent every `PACKET_MS`, holding back the
    /// packets sent during `lag` and delivering them together when it ends. Returns the worst
    /// latency seen.
    fn run(&mut self, ms: usize, frames: usize, lag: Option<(usize, usize)>) -> usize {
        let mut worst = 0;
        let mut held = Vec::new();
        for elapsed in 0..ms {
            if elapsed % PACKET_MS == 0 {
                held.push(self.packet(frames));
            }
            if !lag.is_some_and(|(start, end)| (start..end).contains(&elapsed)) {
                for packet in std::mem::take(&mut held) {
                    self.deliver(&packet);
                }
            }
            worst = worst.max(self.latency_ms());
            self.play_millisecond();
        }
        worst
    }

    fn latency_ms(&self) -> usize {
        self.queue.len() * 1_000 / (SAMPLE_RATE as usize * CHANNELS)
    }
}

#[test]
fn stall_backlog_is_dropped_instead_of_delaying_audio() {
    let mut playback = Playback::new(20);
    playback.run(2_000, PACKET_FRAMES, None);

    playback.run(1_000, PACKET_FRAMES, Some((100, 900)));

    assert!(playback.resyncs > 0);
    let worst = playback.run(5_000, PACKET_FRAMES, None);
    assert!(worst <= 100, "latency after the stall: {worst} ms");
}

#[test]
fn repeated_small_lags_stop_causing_dropouts() {
    const LAG_MS: usize = 120;
    let mut playback = Playback::new(20);
    for _ in 0..10 {
        playback.run(3_000, PACKET_FRAMES, Some((1_000, 1_000 + LAG_MS)));
    }
    let adapted = playback.dropouts;

    let mut worst = 0;
    for _ in 0..10 {
        worst = worst.max(playback.run(3_000, PACKET_FRAMES, Some((1_000, 1_000 + LAG_MS))));
    }

    assert!(adapted <= 4, "dropouts while adapting: {adapted}");
    assert_eq!(playback.dropouts, adapted);
    assert!(worst <= 250, "cushion grew to {worst} ms");
}

#[test]
fn excess_and_clock_drift_drain_without_skips_or_underruns() {
    // The peer's clock runs 0.625% fast and an earlier hiccup left 150 ms queued.
    const FAST_PACKET_FRAMES: usize = PACKET_FRAMES + 3;
    let mut playback = Playback::new(150);

    playback.run(15_000, FAST_PACKET_FRAMES, None);
    let worst = playback.run(45_000, FAST_PACKET_FRAMES, None);

    assert!(worst <= 100, "latency kept growing with drift: {worst} ms");
    assert_eq!(playback.resyncs, 0);
    assert_eq!(playback.dropouts, 0);
}

#[test]
fn steady_audio_plays_unchanged() {
    let mut playback = Playback::new(20);

    playback.run(10_000, PACKET_FRAMES, None);

    assert_eq!(playback.dropouts, 0);
    assert!(!playback.played.is_empty());
    assert!(playback.played == playback.sent[..playback.played.len()]);
}
