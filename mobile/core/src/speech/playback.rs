//! The buffer between synthesis and the audio device.
//!
//! The synthesis thread appends samples and *cues* (marks and utterance ends) at sample
//! positions. The audio callback takes samples out and fires each cue once the samples
//! before it have been handed to the device, so the controller reports marks and ends
//! as the user hears them. When the buffer runs dry, the callback plays silence or, with
//! keep-alive on, inaudible noise that keeps Bluetooth headphones from going to sleep.

use std::collections::VecDeque;
use tokio::sync::mpsc::UnboundedSender;

use rconnect_protocol::messages::SpeechEvent;

/// Amplitude of the keep-alive noise: ±12 out of 32767, about -69 dBFS.
const NOISE_AMPLITUDE: i32 = 12;

pub struct Playback {
    samples: VecDeque<i16>,
    /// Absolute position of the first sample in `samples`.
    played: u64,
    /// `(absolute position, event)`, ordered by position.
    cues: VecDeque<(u64, SpeechEvent)>,
    events: UnboundedSender<SpeechEvent>,
    /// Last mark played, for reporting where a paused utterance stopped.
    last_mark: Option<(u32, u16)>,
    pub keep_alive: bool,
    noise: u32,
}

impl Playback {
    pub fn new(events: UnboundedSender<SpeechEvent>) -> Playback {
        Playback {
            samples: VecDeque::new(),
            played: 0,
            cues: VecDeque::new(),
            events,
            last_mark: None,
            keep_alive: false,
            noise: 0x1234_5678,
        }
    }

    /// Absolute position right after the last queued sample.
    pub fn end_position(&self) -> u64 {
        self.played + self.samples.len() as u64
    }

    /// Whether speech is waiting to be played.
    pub fn is_speaking(&self) -> bool {
        !self.samples.is_empty()
    }

    pub fn push_samples(&mut self, samples: &[i16]) {
        self.samples.extend(samples);
    }

    /// Queues an event to be sent once playback reaches `position`. A position already
    /// played fires with the next audio callback.
    pub fn push_cue(&mut self, position: u64, event: SpeechEvent) {
        let index = self
            .cues
            .partition_point(|(existing, _)| *existing <= position);
        self.cues.insert(index, (position, event));
    }

    /// Drops everything queued. Cues are dropped too: a stopped utterance reports nothing.
    pub fn clear(&mut self) {
        self.played += self.samples.len() as u64;
        self.samples.clear();
        self.cues.clear();
    }

    /// The last mark of utterance `id` that has been played.
    pub fn last_mark(&self, id: u32) -> Option<u16> {
        self.last_mark
            .filter(|(mark_id, _)| *mark_id == id)
            .map(|(_, index)| index)
    }

    /// Fills `out` with the next samples, then fires the cues they reached.
    pub fn fill(&mut self, out: &mut [i16]) {
        let available = out.len().min(self.samples.len());
        for (slot, sample) in out.iter_mut().zip(self.samples.drain(..available)) {
            *slot = sample;
        }
        for slot in &mut out[available..] {
            *slot = if self.keep_alive {
                self.next_noise()
            } else {
                0
            };
        }
        self.played += available as u64;
        // Once the buffer is empty, cues at its end (the utterance's End) are due too.
        let reached = self.played;
        while let Some(&(position, event)) = self.cues.front() {
            if position > reached {
                break;
            }
            self.cues.pop_front();
            if let SpeechEvent::Mark { id, index } = event {
                self.last_mark = Some((id, index));
            }
            let _ = self.events.send(event);
        }
    }

    fn next_noise(&mut self) -> i16 {
        // xorshift32: cheap and allocation free, fine for inaudible noise.
        self.noise ^= self.noise << 13;
        self.noise ^= self.noise >> 17;
        self.noise ^= self.noise << 5;
        ((self.noise % (2 * NOISE_AMPLITUDE as u32 + 1)) as i32 - NOISE_AMPLITUDE) as i16
    }
}

/// Linear resampler from the synthesizer's rate to the device's rate, mono to any number
/// of interleaved channels.
pub struct Resampler {
    /// Input samples per output frame.
    step: f64,
    /// Position between `previous` (0.0) and `current` (1.0).
    phase: f64,
    previous: f32,
    current: f32,
    channels: usize,
    input: Vec<i16>,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32, channels: usize) -> Resampler {
        Resampler {
            step: input_rate as f64 / output_rate as f64,
            phase: 1.0,
            previous: 0.0,
            current: 0.0,
            channels: channels.max(1),
            input: Vec::new(),
        }
    }

    /// Fills interleaved `out` with frames, pulling input from `playback`.
    pub fn fill<T>(&mut self, playback: &mut Playback, out: &mut [T], convert: impl Fn(f32) -> T)
    where
        T: Copy,
    {
        // Count exactly the input these frames consume, so none is pulled and lost.
        let frames = out.len() / self.channels;
        let mut phase = self.phase;
        let mut needed = 0;
        for _ in 0..frames {
            while phase >= 1.0 {
                phase -= 1.0;
                needed += 1;
            }
            phase += self.step;
        }
        self.input.resize(needed, 0);
        playback.fill(&mut self.input);

        let mut next_input = 0;
        for frame in out.chunks_exact_mut(self.channels) {
            while self.phase >= 1.0 {
                self.previous = self.current;
                self.current = self.input.get(next_input).copied().unwrap_or(0) as f32 / 32768.0;
                next_input += 1;
                self.phase -= 1.0;
            }
            let value = self.previous + (self.current - self.previous) * self.phase as f32;
            frame.fill(convert(value));
            self.phase += self.step;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc::{self, UnboundedReceiver};

    fn drain(rx: &mut UnboundedReceiver<SpeechEvent>) -> Vec<SpeechEvent> {
        std::iter::from_fn(|| rx.try_recv().ok()).collect()
    }

    #[test]
    fn cues_fire_when_their_samples_are_played() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut playback = Playback::new(tx);
        playback.push_samples(&[1; 100]);
        playback.push_cue(50, SpeechEvent::Mark { id: 1, index: 0 });
        playback.push_cue(100, SpeechEvent::End { id: 1 });
        playback.push_cue(10, SpeechEvent::Mark { id: 1, index: 9 });

        let mut out = [0i16; 40];
        playback.fill(&mut out);
        assert_eq!(out, [1; 40]);
        assert_eq!(drain(&mut rx), [SpeechEvent::Mark { id: 1, index: 9 }]);

        playback.fill(&mut out);
        assert_eq!(drain(&mut rx), [SpeechEvent::Mark { id: 1, index: 0 }]);
        assert_eq!(playback.last_mark(1), Some(0));

        playback.fill(&mut out);
        assert_eq!(&out[..20], &[1; 20]);
        assert_eq!(&out[20..], &[0; 20]);
        assert_eq!(drain(&mut rx), [SpeechEvent::End { id: 1 }]);
    }

    #[test]
    fn clear_drops_samples_and_cues() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut playback = Playback::new(tx);
        playback.push_samples(&[1; 100]);
        playback.push_cue(100, SpeechEvent::End { id: 1 });
        playback.clear();
        assert_eq!(playback.end_position(), 100);

        let mut out = [5i16; 10];
        playback.fill(&mut out);
        assert_eq!(out, [0; 10]);
        assert!(drain(&mut rx).is_empty());
    }

    #[test]
    fn keep_alive_noise_is_tiny_but_not_silent() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut playback = Playback::new(tx);
        playback.keep_alive = true;
        let mut out = [0i16; 1000];
        playback.fill(&mut out);
        assert!(out.iter().all(|s| s.abs() <= NOISE_AMPLITUDE as i16));
        assert!(out.iter().any(|&s| s != 0));
    }

    #[test]
    fn resampler_keeps_timing() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut playback = Playback::new(tx);
        playback.push_samples(&[16384; 22050]);
        playback.push_cue(22050, SpeechEvent::End { id: 1 });

        let mut resampler = Resampler::new(22050, 48000, 2);
        let mut out = vec![0f32; 2 * 480];
        let mut frames = 0;
        while rx.try_recv().is_err() {
            resampler.fill(&mut playback, &mut out, |v| v);
            frames += 480;
            assert!(frames < 60_000, "End never fired");
        }
        // One second of input ends after about one second of output.
        assert!((47_500..=48_500).contains(&frames), "{frames} frames");
        assert!((out[0] - 0.5).abs() < 0.01 || out[0].abs() < 0.01);
    }
}
