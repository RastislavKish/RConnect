//! Microphone forwarding: the phone's microphone as a microphone on the host (see
//! `docs/ARCHITECTURE.md`, §5.7).
//!
//! [`MicStream`] runs only while the microphone is unmuted (Unmuted or VAD). The capture
//! callback hands samples to an encoder thread, which resamples them to 48 kHz, cuts
//! 20 ms frames, lets the [`vad::VadGate`] pick the ones to send in VAD mode, encodes them
//! with Opus for speech and sends one datagram per frame.

mod capture;
pub mod vad;

use std::sync::mpsc::{self, Receiver};

use iroh::endpoint::{Connection, SendDatagramError};
use rconnect_opus::{Encoder, MAX_PACKET};
use rconnect_protocol::audio::AudioPacket;
use rconnect_protocol::messages::MicMode;

use capture::Capture;
use vad::VadGate;

const RATE: u32 = 48_000;
/// 20 ms.
const FRAME: usize = 960;
const BITRATE: u32 = 24_000;
/// Percent of packets expected to be lost, which sizes Opus's error correction.
const EXPECTED_LOSS: u8 = 5;
/// Captured chunks waiting for the encoder; later ones are dropped.
const CHUNK_QUEUE: usize = 64;

/// The microphone being captured and sent. Dropping it closes the microphone.
pub struct MicStream {
    capture: Option<Capture>,
    modes: mpsc::Sender<MicMode>,
    encoder: Option<std::thread::JoinHandle<()>>,
}

impl MicStream {
    /// Opens the microphone and starts sending in `mode`, Unmuted or VAD. `device_id` picks
    /// the microphone on Android.
    pub fn start(
        connection: Connection,
        mode: MicMode,
        device_id: Option<i32>,
    ) -> anyhow::Result<MicStream> {
        let (chunks_tx, chunks) = mpsc::sync_channel(CHUNK_QUEUE);
        let (capture, rate) = Capture::open(device_id, chunks_tx)?;
        let (modes, modes_rx) = mpsc::channel();
        let encoder = std::thread::Builder::new()
            .name("microphone".into())
            .spawn(move || {
                if let Err(e) = encode(connection, chunks, rate, mode, modes_rx) {
                    tracing::warn!("microphone stream stopped: {e:#}");
                }
            })?;
        tracing::info!("microphone open ({mode:?})");
        Ok(MicStream {
            capture: Some(capture),
            modes,
            encoder: Some(encoder),
        })
    }

    /// Switches between Unmuted and VAD without reopening the microphone.
    pub fn set_mode(&self, mode: MicMode) {
        let _ = self.modes.send(mode);
    }
}

impl Drop for MicStream {
    fn drop(&mut self) {
        // Closing the capture closes the encoder's input, which ends it.
        self.capture.take();
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
        tracing::info!("microphone closed");
    }
}

struct Sender {
    connection: Connection,
    encoder: Encoder,
    packet: Vec<u8>,
    seq: u16,
    timestamp: u32,
}

impl Sender {
    fn send(&mut self, frame: &[f32], end_of_burst: bool) -> anyhow::Result<()> {
        let len = self.encoder.encode(frame, &mut self.packet)?;
        let packet = AudioPacket {
            seq: self.seq,
            timestamp: self.timestamp,
            end_of_burst,
            payload: self.packet[..len].to_vec(),
        };
        self.seq = self.seq.wrapping_add(1);
        match self.connection.send_datagram(packet.encode().into()) {
            Ok(()) | Err(SendDatagramError::TooLarge) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

fn encode(
    connection: Connection,
    chunks: Receiver<Vec<f32>>,
    rate: u32,
    mut mode: MicMode,
    modes: Receiver<MicMode>,
) -> anyhow::Result<()> {
    let mut sender = Sender {
        connection,
        encoder: Encoder::voice(RATE, 1, BITRATE, EXPECTED_LOSS)?,
        packet: vec![0; MAX_PACKET],
        seq: 0,
        timestamp: 0,
    };
    let mut resampler = Resampler::new(rate, RATE);
    let mut gate = VadGate::new();
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * 2);

    while let Ok(chunk) = chunks.recv() {
        while let Ok(new_mode) = modes.try_recv() {
            if mode == MicMode::Unmuted && new_mode == MicMode::Vad {
                // Close the continuous stream as a burst, so the host stops cleanly.
                sender.send(&[0.0; FRAME], true)?;
                gate = VadGate::new();
            }
            if new_mode == MicMode::Unmuted && mode != MicMode::Unmuted {
                sender.encoder.reset();
            }
            mode = new_mode;
        }
        resampler.process(&chunk, &mut pending);
        while pending.len() >= FRAME {
            let frame: Vec<f32> = pending.drain(..FRAME).collect();
            sender.timestamp = sender.timestamp.wrapping_add(FRAME as u32);
            match mode {
                MicMode::Vad => {
                    if let Some(send) = gate.push(frame) {
                        if send.start {
                            sender.encoder.reset();
                        }
                        let last = send.frames.len() - 1;
                        for (index, frame) in send.frames.iter().enumerate() {
                            sender.send(frame, send.end && index == last)?;
                        }
                    }
                }
                _ => sender.send(&frame, false)?,
            }
        }
    }
    Ok(())
}

/// Linear resampler for mono audio, from the microphone's rate to 48 kHz.
struct Resampler {
    /// Input samples per output sample.
    step: f64,
    /// Position of the next output sample, relative to `previous`.
    phase: f64,
    previous: f32,
}

impl Resampler {
    fn new(input_rate: u32, output_rate: u32) -> Resampler {
        Resampler {
            step: input_rate as f64 / output_rate as f64,
            phase: 0.0,
            previous: 0.0,
        }
    }

    fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        if self.step == 1.0 {
            out.extend_from_slice(input);
            return;
        }
        for &current in input {
            while self.phase < 1.0 {
                out.push(self.previous + (current - self.previous) * self.phase as f32);
                self.phase += self.step;
            }
            self.phase -= 1.0;
            self.previous = current;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampler_keeps_the_duration() {
        let mut resampler = Resampler::new(44_100, 48_000);
        let mut out = Vec::new();
        for _ in 0..10 {
            resampler.process(&[0.5; 4410], &mut out);
        }
        // One second in, one second out.
        assert!((47_990..=48_010).contains(&out.len()), "{}", out.len());
    }
}
