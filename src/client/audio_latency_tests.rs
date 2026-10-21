use super::{duration_samples, PlaybackLatency, RESYNC_KEEP_MS};
use std::collections::VecDeque;

const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: usize = 2;
const PACKET_MS: usize = 10;
const PACKET_FRAMES: usize = SAMPLE_RATE as usize * PACKET_MS / 1_000;
const TONE_STEP: f32 = std::f32::consts::TAU * 441.0 / SAMPLE_RATE as f32;

/// A queue drained one millisecond at a time, as an output device would.
struct Playback {
    latency: PlaybackLatency,
    queue: VecDeque<f32>,
    sent: Vec<f32>,
    played: Vec<f32>,
    underruns: usize,
    resyncs: usize,
    phase: usize,
}

impl Playback {
    fn new(prefill_ms: usize) -> Self {
        let mut playback = Self {
            latency: PlaybackLatency::new(SAMPLE_RATE, CHANNELS),
            queue: VecDeque::new(),
            sent: Vec::new(),
            played: Vec::new(),
            underruns: 0,
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
        if self.queue.len() < wanted {
            self.underruns += 1;
        }
        let available = wanted.min(self.queue.len());
        self.played.extend(self.queue.drain(..available));
    }

    /// Runs `ms` of real time with a packet of `frames` sent every `PACKET_MS`, and returns the
    /// worst latency seen.
    fn run(&mut self, ms: usize, frames: usize) -> usize {
        let mut worst = 0;
        for elapsed in 0..ms {
            if elapsed % PACKET_MS == 0 {
                let packet = self.packet(frames);
                self.deliver(&packet);
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
    const STALL_MS: usize = 800;
    let mut playback = Playback::new(20);
    playback.run(2_000, PACKET_FRAMES);

    let mut held = Vec::new();
    for elapsed in 0..STALL_MS {
        if elapsed % PACKET_MS == 0 {
            held.push(playback.packet(PACKET_FRAMES));
        }
        playback.play_millisecond();
    }
    for packet in &held {
        playback.deliver(packet);
    }

    assert!(
        playback.latency_ms() <= RESYNC_KEEP_MS,
        "latency after the burst: {} ms",
        playback.latency_ms()
    );
    let worst = playback.run(5_000, PACKET_FRAMES);
    assert!(worst <= RESYNC_KEEP_MS + PACKET_MS, "latency after the stall: {worst} ms");
}

#[test]
fn excess_and_clock_drift_drain_without_skips_or_underruns() {
    // The peer's clock runs 0.625% fast and an earlier hiccup left 150 ms queued.
    const FAST_PACKET_FRAMES: usize = PACKET_FRAMES + 3;
    let mut playback = Playback::new(150);

    playback.run(10_000, FAST_PACKET_FRAMES);
    let worst = playback.run(50_000, FAST_PACKET_FRAMES);

    assert!(worst <= 80, "latency kept growing with drift: {worst} ms");
    assert_eq!(playback.resyncs, 0);
    assert_eq!(playback.underruns, 0);
}

#[test]
fn steady_audio_plays_unchanged() {
    let mut playback = Playback::new(20);

    playback.run(10_000, PACKET_FRAMES);

    assert_eq!(playback.underruns, 0);
    assert!(!playback.played.is_empty());
    assert!(playback.played == playback.sent[..playback.played.len()]);
}
