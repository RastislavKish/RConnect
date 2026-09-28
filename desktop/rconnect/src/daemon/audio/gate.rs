//! Decides which frames the host sends (see `docs/ARCHITECTURE.md`, §5.6).
//!
//! In Auto mode a frame opens the gate when its peak is above [`THRESHOLD`], and the gate
//! closes after [`HOLD_FRAMES`] frames below it. The frames just before the opening are
//! kept and sent first, so the start of a sound is not cut off. In Always mode every
//! frame is sent.

use std::collections::VecDeque;

use rconnect_protocol::messages::AudioMode;

/// About -60 dBFS.
pub const THRESHOLD: f32 = 0.001;
/// 400 ms of 20 ms frames.
pub const HOLD_FRAMES: u32 = 20;
/// Frames sent ahead of the one that opens the gate.
pub const PRE_ROLL: usize = 2;

/// What to do with the frames handed to [`Gate::push`].
#[derive(Debug, PartialEq)]
pub struct Send {
    /// A burst starts: the encoder should forget the previous one.
    pub start: bool,
    pub frames: Vec<Vec<f32>>,
    /// The last frame closes the burst.
    pub end: bool,
}

pub struct Gate {
    pub mode: AudioMode,
    open: bool,
    quiet_frames: u32,
    pre_roll: VecDeque<Vec<f32>>,
}

impl Gate {
    pub fn new(mode: AudioMode) -> Gate {
        Gate {
            mode,
            open: false,
            quiet_frames: 0,
            pre_roll: VecDeque::new(),
        }
    }

    /// Takes one frame and returns what to send, if anything.
    pub fn push(&mut self, frame: Vec<f32>) -> Option<Send> {
        if self.mode == AudioMode::Always {
            let start = !self.open;
            self.open = true;
            return Some(Send {
                start,
                frames: vec![frame],
                end: false,
            });
        }

        let loud = peak(&frame) > THRESHOLD;
        if !self.open {
            if !loud {
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

        self.quiet_frames = if loud { 0 } else { self.quiet_frames + 1 };
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

    /// Closes an open gate when the audio stops arriving, e.g. because the last
    /// application stopped playing. Returns true if the burst has to be ended.
    pub fn interrupt(&mut self) -> bool {
        self.pre_roll.clear();
        std::mem::replace(&mut self.open, false) && self.mode == AudioMode::Auto
    }
}

fn peak(frame: &[f32]) -> f32 {
    frame
        .iter()
        .fold(0.0, |peak, sample| peak.max(sample.abs()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(level: f32) -> Vec<f32> {
        vec![level; 1920]
    }

    #[test]
    fn auto_opens_with_pre_roll_and_closes_after_the_hold() {
        let mut gate = Gate::new(AudioMode::Auto);
        for _ in 0..5 {
            assert_eq!(gate.push(frame(0.0)), None);
        }
        let send = gate.push(frame(0.5)).unwrap();
        assert!(send.start && !send.end);
        assert_eq!(send.frames.len(), PRE_ROLL + 1);

        for _ in 0..HOLD_FRAMES - 1 {
            let send = gate.push(frame(0.0)).unwrap();
            assert!(!send.start && !send.end);
        }
        let send = gate.push(frame(0.0)).unwrap();
        assert!(send.end);
        assert_eq!(gate.push(frame(0.0)), None);
    }

    #[test]
    fn a_loud_frame_restarts_the_hold() {
        let mut gate = Gate::new(AudioMode::Auto);
        gate.push(frame(0.5));
        for _ in 0..HOLD_FRAMES - 1 {
            gate.push(frame(0.0));
        }
        assert!(!gate.push(frame(0.5)).unwrap().end);
        for _ in 0..HOLD_FRAMES - 1 {
            assert!(!gate.push(frame(0.0)).unwrap().end);
        }
    }

    #[test]
    fn always_sends_silence_too() {
        let mut gate = Gate::new(AudioMode::Always);
        let first = gate.push(frame(0.0)).unwrap();
        assert!(first.start);
        let second = gate.push(frame(0.0)).unwrap();
        assert!(!second.start && !second.end);
        assert!(!gate.interrupt(), "Always never ends a burst");
    }

    #[test]
    fn interrupt_ends_an_open_burst_once() {
        let mut gate = Gate::new(AudioMode::Auto);
        gate.push(frame(0.5));
        assert!(gate.interrupt());
        assert!(!gate.interrupt());
    }
}
