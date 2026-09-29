//! The audio stream to the controller (see `docs/ARCHITECTURE.md`, §5.6).
//!
//! [`capture`] delivers what applications play. The encoder thread cuts it into 20 ms
//! frames, lets [`gate`] pick the ones to send, encodes them with Opus and sends one
//! datagram per frame. Nothing here runs unless the controller asked for audio.

mod capture;
mod gate;
mod mic;
mod mute;

use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, SendDatagramError};
use rconnect_opus::{Encoder, MAX_PACKET};
use rconnect_protocol::audio::AudioPacket;
use rconnect_protocol::messages::{AudioCodec, AudioDirection, AudioFormat, AudioMode};

use capture::Capture;
pub use capture::CaptureKind;
use gate::Gate;
pub use mic::VirtualMic;
use mute::LocalMute;
pub use mute::restore_after_crash;

pub const RATE: u32 = 48_000;
pub const CHANNELS: usize = 2;
const FRAME_MS: u8 = 20;
const FRAME: usize = RATE as usize * FRAME_MS as usize / 1000;
/// Bitrates on a direct and on a relayed path.
const BITRATE_DIRECT: u32 = 64_000;
const BITRATE_RELAYED: u32 = 32_000;
/// How long captured audio may stop arriving before the gate counts it as silence.
const STALL: Duration = Duration::from_millis(60);
/// Captured chunks waiting for the encoder; later ones are dropped.
const PCM_QUEUE: usize = 32;
/// Frames between checks of the connection's path.
const PATH_CHECK_FRAMES: u32 = 50;

/// Largest number of outgoing datagram bytes QUIC buffers. When the link cannot keep up,
/// the oldest audio is dropped rather than queued ahead of speech.
pub const DATAGRAM_SEND_BUFFER: usize = 2048;

/// A running audio stream. Dropping it stops the capture.
pub struct AudioStream {
    capture: Option<Capture>,
    modes: mpsc::Sender<AudioMode>,
    encoder: Option<std::thread::JoinHandle<()>>,
    /// Unmutes the computer once the capture has stopped, if it was muted.
    _mute: Option<LocalMute>,
}

impl AudioStream {
    /// Starts streaming. `mute_locally` keeps the computer silent meanwhile; it needs
    /// [`CaptureKind::Streams`], whose taps come before the sink's mute.
    pub fn start(
        connection: Connection,
        mode: AudioMode,
        kind: CaptureKind,
        mute_locally: bool,
    ) -> anyhow::Result<AudioStream> {
        if connection.max_datagram_size().is_none() {
            anyhow::bail!("the controller does not accept datagrams");
        }
        let (pcm_tx, pcm_rx) = mpsc::sync_channel(PCM_QUEUE);
        let capture = Capture::start(kind, pcm_tx)?;
        let (modes, modes_rx) = mpsc::channel();
        let encoder = std::thread::Builder::new()
            .name("audio encoder".into())
            .spawn(move || {
                if let Err(e) = encode(connection, pcm_rx, mode, modes_rx) {
                    tracing::warn!("audio stream stopped: {e:#}");
                }
            })?;
        tracing::info!("streaming audio ({mode:?}, {kind:?})");
        Ok(AudioStream {
            capture: Some(capture),
            modes,
            encoder: Some(encoder),
            _mute: (mute_locally && kind == CaptureKind::Streams)
                .then(LocalMute::mute_default_sink)
                .flatten(),
        })
    }

    pub fn set_mode(&self, mode: AudioMode) {
        let _ = self.modes.send(mode);
    }

    pub fn format(mode: AudioMode) -> AudioFormat {
        AudioFormat {
            direction: AudioDirection::ToController,
            mode,
            codec: AudioCodec::Opus,
            sample_rate: RATE,
            channels: CHANNELS as u8,
            frame_ms: FRAME_MS,
        }
    }
}

impl Drop for AudioStream {
    fn drop(&mut self) {
        // Stopping the capture closes the encoder's input, which ends it.
        self.capture.take();
        if let Some(encoder) = self.encoder.take() {
            let _ = encoder.join();
        }
        tracing::info!("audio stream stopped");
    }
}

struct Sender {
    connection: Connection,
    encoder: Encoder,
    packet: Vec<u8>,
    seq: u16,
    /// Samples per channel since the stream started, gated frames included.
    timestamp: u32,
    frames_until_path_check: u32,
}

impl Sender {
    fn send(&mut self, frame: &[f32], end_of_burst: bool) -> anyhow::Result<()> {
        if self.frames_until_path_check == 0 {
            self.frames_until_path_check = PATH_CHECK_FRAMES;
            let relayed = self
                .connection
                .paths()
                .iter()
                .any(|path| path.is_selected() && path.is_relay());
            let bitrate = if relayed {
                BITRATE_RELAYED
            } else {
                BITRATE_DIRECT
            };
            self.encoder.set_bitrate(bitrate)?;
        }
        self.frames_until_path_check -= 1;

        let len = self.encoder.encode(frame, &mut self.packet)?;
        let packet = AudioPacket {
            seq: self.seq,
            timestamp: self.timestamp,
            end_of_burst,
            payload: self.packet[..len].to_vec(),
        };
        self.seq = self.seq.wrapping_add(1);
        match self.connection.send_datagram(packet.encode().into()) {
            Ok(()) => Ok(()),
            Err(SendDatagramError::TooLarge) => {
                tracing::debug!("audio frame of {len} bytes does not fit a datagram");
                Ok(())
            }
            Err(e) => Err(e.into()),
        }
    }

    fn apply(&mut self, frames: gate::Send) -> anyhow::Result<()> {
        if frames.start {
            self.encoder.reset();
        }
        let last = frames.frames.len() - 1;
        for (index, frame) in frames.frames.iter().enumerate() {
            self.send(frame, frames.end && index == last)?;
        }
        Ok(())
    }
}

fn encode(
    connection: Connection,
    pcm: Receiver<Vec<f32>>,
    mode: AudioMode,
    modes: Receiver<AudioMode>,
) -> anyhow::Result<()> {
    let mut sender = Sender {
        connection,
        encoder: Encoder::new(RATE, CHANNELS as u8, BITRATE_DIRECT)?,
        packet: vec![0; MAX_PACKET],
        seq: 0,
        timestamp: 0,
        frames_until_path_check: 0,
    };
    let mut gate = Gate::new(mode);
    let mut pending: Vec<f32> = Vec::with_capacity(FRAME * CHANNELS * 2);
    let silence = vec![0.0; FRAME * CHANNELS];
    let mut last_audio = Instant::now();
    // In Always mode, when to send the next frame of made-up silence.
    let mut next_silence = Instant::now();

    loop {
        while let Ok(mode) = modes.try_recv() {
            gate.mode = mode;
        }
        match pcm.recv_timeout(Duration::from_millis(FRAME_MS as u64)) {
            Ok(chunk) => {
                last_audio = Instant::now();
                pending.extend_from_slice(&chunk);
                while pending.len() >= FRAME * CHANNELS {
                    let frame: Vec<f32> = pending.drain(..FRAME * CHANNELS).collect();
                    sender.timestamp = sender.timestamp.wrapping_add(FRAME as u32);
                    if let Some(send) = gate.push(frame) {
                        sender.apply(send)?;
                    }
                }
                next_silence = Instant::now() + STALL;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }

        // Nothing is captured while no application plays: the capture node is passive.
        if last_audio.elapsed() < STALL {
            continue;
        }
        pending.clear();
        match gate.mode {
            AudioMode::Auto => {
                if gate.interrupt() {
                    sender.send(&silence, true)?;
                }
            }
            AudioMode::Always => {
                // Keep the stream continuous with silence at the frame rate, starting
                // now: a gap that already passed is not made up in a burst.
                let now = Instant::now();
                if next_silence + Duration::from_millis(FRAME_MS as u64) < now {
                    next_silence = now;
                }
                while next_silence <= now {
                    next_silence += Duration::from_millis(FRAME_MS as u64);
                    sender.timestamp = sender.timestamp.wrapping_add(FRAME as u32);
                    if let Some(send) = gate.push(silence.clone()) {
                        sender.apply(send)?;
                    }
                }
            }
        }
    }
}
