//! Audio frames get their own queue on the connection, sent ahead of video and every other
//! message, so a busy link delays the picture instead of breaking up the sound. Audio is a few
//! kilobytes a second, so serving it first cannot starve anything else.

use super::*;

pub(super) type AudioQueue = mpsc::UnboundedReceiver<(Instant, Arc<Message>)>;

/// Audio this far behind is already out of step with the video and is not worth sending.
const STALE_AUDIO_MS: u128 = 1_000;

impl Connection {
    /// Sends every audio frame already waiting, before anything else gets a turn.
    pub(super) async fn send_queued_audio(&mut self, audio: &mut AudioQueue) -> bool {
        while let Ok(frame) = audio.try_recv() {
            if !self.send_audio(frame).await {
                return false;
            }
        }
        true
    }

    pub(super) async fn send_audio(&mut self, (queued_at, msg): (Instant, Arc<Message>)) -> bool {
        if queued_at.elapsed().as_millis() > STALE_AUDIO_MS {
            return true;
        }
        if let Err(err) = self.stream.send(&msg as &Message).await {
            self.on_close(&err.to_string(), false).await;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_frames_take_their_own_lane() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (tx_video, mut rx_video) = mpsc::unbounded_channel();
        let (tx_audio, mut rx_audio) = mpsc::unbounded_channel();
        let mut inner = ConnInner {
            id: 1,
            tx: Some(tx),
            tx_video: Some(tx_video),
            tx_audio: Some(tx_audio),
        };
        let mut audio = Message::new();
        audio.set_audio_frame(AudioFrame::new());
        let mut video = Message::new();
        video.set_video_frame(VideoFrame::new());

        inner.send(Arc::new(video));
        inner.send(Arc::new(audio));

        assert!(rx_audio.try_recv().is_ok());
        assert!(rx_video.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn audio_stays_on_the_shared_queue_without_a_lane() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut inner = ConnInner::new(1, Some(tx), None);
        let mut audio = Message::new();
        audio.set_audio_frame(AudioFrame::new());

        inner.send(Arc::new(audio));

        assert!(rx.try_recv().is_ok());
    }
}
