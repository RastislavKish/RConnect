//! Decides which microphone frames to send in VAD mode (`docs/ARCHITECTURE.md`, §5.7).
//!
//! Earshot scores 16 ms frames at 16 kHz; each 20 ms Opus frame counts as speech when a
//! score within it reached [`THRESHOLD`]. Speech opens a burst, which starts with the
//! [`PRE_ROLL`] frames before it, so soft onsets are kept, and ends [`HOLD_FRAMES`]
//! frames after the last speech, so pauses between words pass.

use std::collections::VecDeque;

use earshot::Detector;

pub const THRESHOLD: f32 = 0.5;
/// 60 ms of 20 ms frames.
pub const PRE_ROLL: usize = 3;
/// 400 ms of 20 ms frames.
pub const HOLD_FRAMES: u32 = 20;
/// Earshot's frame: 16 ms at 16 kHz.
const DETECTOR_FRAME: usize = 256;

/// What to send for one frame handed to [`VadGate::push`].
#[derive(Debug, PartialEq)]
pub struct Send {
    /// A burst starts: the encoder should forget the previous one.
    pub start: bool,
    pub frames: Vec<Vec<f32>>,
    /// The last frame closes the burst.
    pub end: bool,
}

pub struct VadGate {
    detector: Detector,
    /// 16 kHz samples waiting for a full detector frame.
    pending: Vec<f32>,
    open: bool,
    quiet_frames: u32,
    pre_roll: VecDeque<Vec<f32>>,
}

impl Default for VadGate {
    fn default() -> VadGate {
        VadGate::new()
    }
}

impl VadGate {
    pub fn new() -> VadGate {
        VadGate {
            detector: Detector::default(),
            pending: Vec::with_capacity(DETECTOR_FRAME * 2),
            open: false,
            quiet_frames: 0,
            pre_roll: VecDeque::new(),
        }
    }

    /// Takes one mono 48 kHz frame and returns what to send, if anything.
    pub fn push(&mut self, frame: Vec<f32>) -> Option<Send> {
        let speech = self.detect(&frame);
        self.decide(frame, speech)
    }

    /// Whether Earshot heard speech in `frame`, which it sees downsampled to 16 kHz.
    fn detect(&mut self, frame: &[f32]) -> bool {
        // Averaging three samples is a crude low-pass filter, enough for detection.
        self.pending.extend(
            frame
                .as_chunks::<3>()
                .0
                .iter()
                .map(|three| three.iter().sum::<f32>() / 3.0),
        );
        let mut speech = false;
        while self.pending.len() >= DETECTOR_FRAME {
            let score = self.detector.predict_f32(&self.pending[..DETECTOR_FRAME]);
            speech |= score >= THRESHOLD;
            self.pending.drain(..DETECTOR_FRAME);
        }
        speech
    }

    fn decide(&mut self, frame: Vec<f32>, speech: bool) -> Option<Send> {
        if !self.open {
            if !speech {
                self.pre_roll.push_back(frame);
                if self.pre_roll.len() > PRE_ROLL {
                    self.pre_roll.pop_front();
                }
                return None;
            }
            self.open = true;
            self.quiet_frames = 0;
            let mut frames: Vec<Vec<f32>> = self.pre_roll.drain(..).collect();
            frames.push(frame);
            return Some(Send {
                start: true,
                frames,
                end: false,
            });
        }
        self.quiet_frames = if speech { 0 } else { self.quiet_frames + 1 };
        let end = self.quiet_frames >= HOLD_FRAMES;
        if end {
            self.open = false;
        }
        Some(Send {
            start: false,
            frames: vec![frame],
            end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Vec<f32> {
        vec![0.0; 960]
    }

    #[test]
    fn speech_opens_with_pre_roll_and_closes_after_the_hold() {
        let mut gate = VadGate::new();
        for _ in 0..5 {
            assert_eq!(gate.decide(frame(), false), None);
        }
        let send = gate.decide(frame(), true).unwrap();
        assert!(send.start && !send.end);
        assert_eq!(send.frames.len(), PRE_ROLL + 1);
        for _ in 0..HOLD_FRAMES - 1 {
            assert!(!gate.decide(frame(), false).unwrap().end);
        }
        assert!(gate.decide(frame(), false).unwrap().end);
        assert_eq!(gate.decide(frame(), false), None);
    }

    #[test]
    fn silence_is_not_speech() {
        let mut gate = VadGate::new();
        for _ in 0..20 {
            assert_eq!(gate.push(frame()), None);
        }
    }
}
