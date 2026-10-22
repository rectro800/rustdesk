use super::audio_latency::{self, PlaybackFeedback, PlaybackLatency};
use hbb_common::{log, ResultType};
use pulse::{def::BufferAttr, sample::Spec, stream::Direction};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};

const WRITE_CHUNK_MS: usize = 10;
// Small enough that the server holds little audio we cannot trim, large enough to ride out
// scheduling delays of the writer thread.
const SERVER_BUFFER_MS: usize = 50;
const UNDERRUN_DECLICK_MS: usize = 5;
const MILLISECONDS_PER_SECOND: usize = 1_000;
const LOG_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Default)]
struct PcmQueue {
    samples: VecDeque<f32>,
    discontinuity: bool,
}

#[derive(Default)]
struct Shared {
    queue: Mutex<PcmQueue>,
    stop: AtomicBool,
}

/// PulseAudio playback fed through a queue the decoder never blocks on.
///
/// `pa_simple_write` blocks once the server buffer is full, which used to park a stall's backlog
/// in the decoder channel where nothing could see or drop it.
pub(super) struct PulsePlayback {
    shared: Arc<Shared>,
    latency: PlaybackLatency,
}

impl PulsePlayback {
    pub(super) fn new(spec: Spec) -> ResultType<Self> {
        let latency = PlaybackLatency::new(spec.rate, spec.channels as _);
        let feedback = latency.feedback();
        let shared = Arc::new(Shared::default());
        let writer_shared = shared.clone();
        let (started_tx, started_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("audio-playback".to_owned())
            .spawn(move || {
                let simple = match open_stream(&spec) {
                    Ok(simple) => {
                        started_tx.send(Ok(())).ok();
                        simple
                    }
                    Err(error) => {
                        started_tx.send(Err(error)).ok();
                        return;
                    }
                };
                write_queue(&writer_shared, &simple, &spec, &feedback);
            })?;
        started_rx.recv()??;
        Ok(Self { shared, latency })
    }

    pub(super) fn push(&mut self, pcm: &[f32]) {
        let mut queue = self.shared.queue.lock().unwrap();
        let plan = self.latency.plan(queue.samples.len(), pcm);
        if let Some(keep) = plan.keep_queued {
            let dropped = queue.samples.len().saturating_sub(keep);
            if dropped > 0 {
                queue.samples.drain(..dropped);
                queue.discontinuity = true;
                hbb_common::throttled_log!(
                    LOG_INTERVAL,
                    info,
                    "Audio playback resynced after a stall: dropped {dropped} samples"
                );
            }
        }
        queue.samples.extend(self.latency.chunk(pcm, &plan));
    }
}

impl Drop for PulsePlayback {
    fn drop(&mut self) {
        // Not joined: the writer may sit in a blocking write, and it exits right after it.
        self.shared.stop.store(true, Ordering::Relaxed);
    }
}

fn open_stream(spec: &Spec) -> ResultType<psimple::Simple> {
    let bytes = |ms| {
        (audio_latency::duration_samples(spec.rate, spec.channels as _, ms)
            * std::mem::size_of::<f32>()) as u32
    };
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: bytes(SERVER_BUFFER_MS),
        prebuf: u32::MAX,
        minreq: bytes(WRITE_CHUNK_MS),
        fragsize: u32::MAX,
    };
    Ok(psimple::Simple::new(
        None,                   // Use the default server
        &crate::get_app_name(), // Our application’s name
        Direction::Playback,    // We want a playback stream
        None,                   // Use the default device
        "playback",             // Description of our stream
        spec,                   // Our sample format
        None,                   // Use default channel map
        Some(&attr),
    )?)
}

/// Writes the queue to the server in fixed chunks, padding with silence while it is short so the
/// server clock, not packet arrival, paces playback.
fn write_queue(
    shared: &Shared,
    simple: &psimple::Simple,
    spec: &Spec,
    feedback: &PlaybackFeedback,
) {
    let channels = usize::from(spec.channels).max(1);
    let chunk_samples =
        audio_latency::duration_samples(spec.rate, channels, WRITE_CHUNK_MS).max(channels);
    let mut output = vec![0.0_f32; chunk_samples];
    let mut declick = Declick::new(spec.rate, channels);
    let mut starved = true;
    while !shared.stop.load(Ordering::Relaxed) {
        let available = {
            let mut queue = shared.queue.lock().unwrap();
            if std::mem::take(&mut queue.discontinuity) {
                declick.begin_discontinuity();
            }
            let queued = queue.samples.len();
            if starved && queued < feedback.resume_samples().max(chunk_samples) {
                0
            } else {
                let available = queued.min(chunk_samples) / channels * channels;
                for (slot, sample) in output.iter_mut().zip(queue.samples.drain(..available)) {
                    *slot = sample;
                }
                let ran_dry = available < chunk_samples;
                if ran_dry && !starved && declick.is_audible() {
                    feedback.report_underrun();
                }
                starved = ran_dry;
                available
            }
        };
        declick.apply(&mut output, available);
        let bytes = unsafe {
            std::slice::from_raw_parts(
                output.as_ptr() as *const u8,
                output.len() * std::mem::size_of::<f32>(),
            )
        };
        if let Err(error) = simple.write(bytes) {
            log::error!("Audio playback write failed: {error}");
            return;
        }
    }
}

/// Ramps between input and silence, and across dropped audio, instead of stepping.
struct Declick {
    channels: usize,
    transition_frames: usize,
    transition_frame: usize,
    had_input: bool,
    transition_start: Vec<f32>,
    last_output: Vec<f32>,
}

impl Declick {
    fn new(sample_rate: u32, channels: usize) -> Self {
        let transition_frames =
            (sample_rate as usize * UNDERRUN_DECLICK_MS / MILLISECONDS_PER_SECOND).max(1);
        Self {
            channels,
            transition_frames,
            transition_frame: transition_frames,
            had_input: false,
            transition_start: vec![0.0; channels],
            last_output: vec![0.0; channels],
        }
    }

    fn is_audible(&self) -> bool {
        self.last_output
            .iter()
            .any(|sample| sample.abs() >= audio_latency::QUIET_PEAK)
    }

    fn begin_discontinuity(&mut self) {
        self.transition_start.copy_from_slice(&self.last_output);
        self.transition_frame = 0;
    }

    /// The first `available` samples of `output` are input; the rest becomes silence.
    fn apply(&mut self, output: &mut [f32], available: usize) {
        for (index, frame) in output.chunks_exact_mut(self.channels).enumerate() {
            let has_input = (index + 1) * self.channels <= available;
            if has_input != self.had_input {
                self.begin_discontinuity();
                self.had_input = has_input;
            }
            let weight = if self.transition_frame >= self.transition_frames {
                1.0
            } else {
                self.transition_frame += 1;
                self.transition_frame as f32 / self.transition_frames as f32
            };
            for (channel, sample) in frame.iter_mut().enumerate() {
                let target = if has_input { *sample } else { 0.0 };
                *sample = self.transition_start[channel] * (1.0 - weight) + target * weight;
                self.last_output[channel] = *sample;
            }
        }
    }
}
