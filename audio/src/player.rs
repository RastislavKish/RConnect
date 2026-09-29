//! Plays an audio stream received as datagrams.
//!
//! Datagrams go into a jitter buffer. The audio callback decodes them as it needs audio,
//! recovers or conceals lost frames, and resamples the result to the device's rate,
//! adjusting the ratio slightly to follow the sender's clock. The result is added to the
//! callback's buffer: on the phone on top of speech, never queued behind it, so speech
//! latency is unaffected.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use rconnect_opus::Decoder;
use rconnect_protocol::audio::AudioPacket;

/// Opus always decodes to this rate, whatever the sender encoded.
const RATE: u32 = 48_000;
const CHANNELS: usize = 2;
/// The longest Opus frame, 120 ms.
const MAX_FRAME: usize = 5760;
/// Assumed frame length until the first packet has been decoded, 20 ms.
const DEFAULT_FRAME: usize = 960;

/// Packets to hold before a burst starts playing, and the bounds of its adaptation.
const MIN_TARGET: usize = 2;
const MAX_TARGET: usize = 6;
/// Packets beyond which the oldest are dropped: one second at 20 ms.
const MAX_BUFFERED: usize = 50;
/// Missing packets concealed in a row before playback stops and buffers again.
const MAX_CONCEALED: u32 = 3;
/// Seconds without an underrun after which the target shrinks by one packet.
const STABLE_SECONDS: u32 = 30;

/// Largest speed-up or slow-down used to follow the sender's clock: ±0.4 %.
const MAX_CORRECTION: f64 = 0.004;
/// Correction per packet of distance from the target.
const CORRECTION_PER_PACKET: f64 = 0.001;

/// Gain of the stream while speech plays, when ducking is on.
const DUCKED_GAIN: f32 = 0.3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// No burst; nothing plays.
    Idle,
    /// Collecting packets before playing. `fresh` when a new burst starts, rather than
    /// after an underrun, so the decoder forgets the previous burst.
    Buffering {
        fresh: bool,
    },
    Playing,
}

pub struct StreamPlayer {
    inner: Mutex<Inner>,
}

struct Inner {
    /// What is played, for the log, e.g. `host audio`.
    name: &'static str,
    decoder: Option<Decoder>,
    state: State,
    /// Packets by extended sequence number (see [`Inner::extend`]).
    packets: BTreeMap<u64, AudioPacket>,
    /// Highest extended sequence number received, for unwrapping the next one.
    newest: Option<u64>,
    /// Next packet to play; older ones arriving now are too late.
    next: u64,
    /// Decoded, interleaved stereo samples at [`RATE`].
    pcm: VecDeque<f32>,
    decoded: Vec<f32>,
    frame_len: usize,
    concealed: u32,
    target: usize,
    /// Output frames played since the last underrun.
    stable_frames: u64,
    /// Smoothed buffer level in packets, for the clock correction.
    level: f64,
    // Resampler: position between `previous` (0.0) and `current` (1.0).
    phase: f64,
    previous: [f32; 2],
    current: [f32; 2],
    gain: f32,
    volume: f32,
    ducking: bool,
    /// Sum of squares and frames mixed since the last level trace.
    trace_energy: f64,
    trace_frames: u64,
}

impl StreamPlayer {
    /// `name` says what is played, for the log, e.g. `host audio`.
    pub fn new(name: &'static str, volume: u8, ducking: bool) -> StreamPlayer {
        StreamPlayer {
            inner: Mutex::new(Inner {
                name,
                decoder: None,
                state: State::Idle,
                packets: BTreeMap::new(),
                newest: None,
                next: 0,
                pcm: VecDeque::new(),
                decoded: vec![0.0; MAX_FRAME * CHANNELS],
                frame_len: DEFAULT_FRAME,
                concealed: 0,
                target: MIN_TARGET,
                stable_frames: 0,
                level: 0.0,
                phase: 1.0,
                previous: [0.0; 2],
                current: [0.0; 2],
                gain: 0.0,
                volume: volume_gain(volume),
                ducking,
                trace_energy: 0.0,
                trace_frames: 0,
            }),
        }
    }

    /// Volume of the stream, 0..=100.
    pub fn set_volume(&self, volume: u8) {
        self.inner.lock().unwrap().volume = volume_gain(volume);
    }

    /// Whether the stream is turned down while speech plays.
    pub fn set_ducking(&self, ducking: bool) {
        self.inner.lock().unwrap().ducking = ducking;
    }

    /// A datagram from the sender.
    pub fn push(&self, datagram: &[u8]) {
        let Some(packet) = AudioPacket::decode(datagram) else {
            return;
        };
        self.inner.lock().unwrap().push(packet);
    }

    /// Drops everything buffered, e.g. when the stream stops or starts again. A new
    /// stream numbers its packets from zero.
    pub fn reset(&self) {
        self.inner.lock().unwrap().reset();
    }

    /// Adds the stream to `out`, `channels` interleaved channels at `device_rate`.
    /// `speaking` tells whether speech plays in this buffer, for ducking.
    pub fn mix(&self, out: &mut [f32], channels: usize, device_rate: u32, speaking: bool) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.mix(out, channels.max(1), device_rate, speaking);
    }
}

fn volume_gain(volume: u8) -> f32 {
    volume.min(100) as f32 / 100.0
}

impl Inner {
    /// Unwraps a 16-bit sequence number into a monotonic one, relative to the newest.
    fn extend(&self, seq: u16) -> u64 {
        match self.newest {
            // Start high, so a slightly older packet does not go below zero.
            None => (1 << 32) + seq as u64,
            Some(newest) => {
                let delta = seq.wrapping_sub(newest as u16) as i16;
                newest.wrapping_add_signed(delta as i64)
            }
        }
    }

    fn reset(&mut self) {
        self.state = State::Idle;
        self.packets.clear();
        self.newest = None;
        self.next = 0;
        self.pcm.clear();
        self.concealed = 0;
        self.level = 0.0;
    }

    fn push(&mut self, packet: AudioPacket) {
        let mut seq = self.extend(packet.seq);
        // Far from everything recent: the sender started a new stream, and this packet
        // overtook the message that says so.
        if let Some(newest) = self.newest
            && seq.abs_diff(newest) > 2 * MAX_BUFFERED as u64
        {
            tracing::debug!("{} numbering jumped; starting over", self.name);
            self.reset();
            seq = self.extend(packet.seq);
        }
        let playing = matches!(
            self.state,
            State::Playing | State::Buffering { fresh: false }
        );
        if playing && seq < self.next {
            return; // Too late: its time has passed.
        }
        self.newest = Some(self.newest.map_or(seq, |newest| newest.max(seq)));
        self.packets.insert(seq, packet);
        while self.packets.len() > MAX_BUFFERED {
            self.packets.pop_first();
        }
        match self.state {
            State::Idle => self.state = State::Buffering { fresh: true },
            State::Buffering { .. } | State::Playing => {}
        }
    }

    /// Decodes until `pcm` holds `frames` frames, or nothing more can be played.
    fn fill_pcm(&mut self, frames: usize) {
        while self.pcm.len() < frames * CHANNELS {
            match self.state {
                State::Idle => return,
                State::Buffering { fresh } => {
                    if self.packets.len() < self.target {
                        return;
                    }
                    if fresh && let Some(decoder) = &mut self.decoder {
                        decoder.reset();
                    }
                    self.next = *self.packets.keys().next().unwrap();
                    self.concealed = 0;
                    self.state = State::Playing;
                    tracing::debug!(
                        "{} plays ({} packets buffered, fresh burst: {fresh})",
                        self.name,
                        self.packets.len()
                    );
                }
                State::Playing => self.decode_next(),
            }
        }
    }

    fn decode_next(&mut self) {
        if self.decoder.is_none() {
            match Decoder::new(RATE, CHANNELS as u8) {
                Ok(decoder) => self.decoder = Some(decoder),
                Err(e) => {
                    tracing::warn!("cannot decode {}: {e}", self.name);
                    self.state = State::Idle;
                    return;
                }
            }
        }
        let decoder = self.decoder.as_mut().unwrap();
        let seq = self.next;
        self.next += 1;

        if let Some(packet) = self.packets.remove(&seq) {
            self.concealed = 0;
            match decoder.decode(Some(&packet.payload), &mut self.decoded) {
                Ok(frames) => {
                    self.frame_len = frames;
                    self.pcm.extend(&self.decoded[..frames * CHANNELS]);
                }
                Err(e) => tracing::debug!("dropping undecodable {}: {e}", self.name),
            }
            if packet.end_of_burst {
                tracing::debug!("{} burst ended", self.name);
                self.state = State::Idle;
                self.packets.clear();
            }
            return;
        }

        // Missing. With later packets waiting it was lost; otherwise the buffer ran dry.
        let lost = self.packets.keys().next().is_some_and(|&later| later > seq);
        if !lost && self.concealed >= MAX_CONCEALED {
            // Stop and buffer again with more margin. A lost end of burst ends up here too.
            self.state = State::Buffering { fresh: false };
            self.target = (self.target + 1).min(MAX_TARGET);
            self.stable_frames = 0;
            tracing::debug!("{} ran dry; buffering {} packets", self.name, self.target);
            return;
        }
        let frames = self.frame_len;
        // The next packet may carry this one's forward error correction data (§5.7).
        if lost
            && let Some(next) = self.packets.get(&(seq + 1))
            && let Ok(recovered) =
                decoder.recover(&next.payload, &mut self.decoded[..frames * CHANNELS])
        {
            self.concealed = 0;
            self.pcm.extend(&self.decoded[..recovered * CHANNELS]);
            return;
        }
        self.concealed += 1;
        match decoder.decode(None, &mut self.decoded[..frames * CHANNELS]) {
            Ok(frames) => {
                // Fade concealment out, so a gap does not end in a loop of guesswork.
                let fade = 1.0 - self.concealed as f32 / (MAX_CONCEALED + 1) as f32;
                self.pcm
                    .extend(self.decoded[..frames * CHANNELS].iter().map(|s| s * fade));
            }
            Err(e) => {
                tracing::debug!("cannot conceal lost {}: {e}", self.name);
                self.pcm.extend(std::iter::repeat_n(0.0, frames * CHANNELS));
            }
        }
    }

    fn mix(&mut self, out: &mut [f32], channels: usize, device_rate: u32, speaking: bool) {
        let target_gain = if speaking && self.ducking {
            self.volume * DUCKED_GAIN
        } else {
            self.volume
        };
        let silent = self.state == State::Idle && self.pcm.is_empty();
        if silent && self.gain == 0.0 {
            return; // Nothing streams: the common case costs nothing.
        }

        let frames = out.len() / channels;
        let step = RATE as f64 / device_rate as f64 * (1.0 + self.clock_correction());
        let needed = (frames as f64 * step).ceil() as usize + 1;
        self.fill_pcm(needed);
        if self.state == State::Playing {
            self.stable_frames += frames as u64;
            if self.stable_frames > STABLE_SECONDS as u64 * device_rate as u64
                && self.target > MIN_TARGET
            {
                self.target -= 1;
                self.stable_frames = 0;
            }
        }

        // Ramp the gain across the buffer, to avoid clicks when it changes.
        let gain_step = (target_gain - self.gain) / frames.max(1) as f32;
        for frame in out.chunks_exact_mut(channels) {
            while self.phase >= 1.0 {
                self.previous = self.current;
                self.current = match (self.pcm.pop_front(), self.pcm.pop_front()) {
                    (Some(left), Some(right)) => [left, right],
                    _ => [0.0, 0.0],
                };
                self.phase -= 1.0;
            }
            let t = self.phase as f32;
            let left = self.previous[0] + (self.current[0] - self.previous[0]) * t;
            let right = self.previous[1] + (self.current[1] - self.previous[1]) * t;
            self.gain += gain_step;
            self.trace_energy += ((left * left + right * right) * 0.5) as f64;
            match frame {
                [mono] => *mono += (left + right) * 0.5 * self.gain,
                [l, r, ..] => {
                    *l += left * self.gain;
                    *r += right * self.gain;
                }
                [] => {}
            }
            self.phase += step;
        }
        self.gain = target_gain;
        self.trace_frames += frames as u64;
        if self.trace_frames >= device_rate as u64 {
            tracing::trace!(
                "{}: level {:.4}, {:?}, {} packets buffered",
                self.name,
                (self.trace_energy / self.trace_frames as f64).sqrt(),
                self.state,
                self.packets.len()
            );
            self.trace_energy = 0.0;
            self.trace_frames = 0;
        }
        if self.state == State::Idle && self.pcm.is_empty() {
            // The burst is over; let the next one start from a clean resampler.
            self.previous = [0.0; 2];
            self.current = [0.0; 2];
            self.gain = 0.0;
        }
    }

    /// How much faster than nominal to consume the stream, to keep the buffer at its
    /// target despite the sender's and the receiver's clocks running at slightly different
    /// speeds.
    fn clock_correction(&mut self) -> f64 {
        if self.state != State::Playing {
            return 0.0;
        }
        let level =
            (self.pcm.len() / CHANNELS) as f64 / self.frame_len as f64 + self.packets.len() as f64;
        self.level += (level - self.level) * 0.05;
        ((self.level - self.target as f64) * CORRECTION_PER_PACKET)
            .clamp(-MAX_CORRECTION, MAX_CORRECTION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rconnect_opus::{Encoder, MAX_PACKET};

    struct Host {
        encoder: Encoder,
        seq: u16,
        frame: usize,
    }

    impl Host {
        fn new() -> Host {
            Host {
                encoder: Encoder::new(RATE, 2, 64_000).unwrap(),
                seq: 0,
                frame: 0,
            }
        }

        /// One 20 ms frame of a 440 Hz tone.
        fn packet(&mut self, end_of_burst: bool) -> Vec<u8> {
            let pcm: Vec<f32> = (0..DEFAULT_FRAME)
                .flat_map(|i| {
                    let t = (self.frame * DEFAULT_FRAME + i) as f32 / RATE as f32;
                    let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    [v, v]
                })
                .collect();
            let mut payload = vec![0; MAX_PACKET];
            let len = self.encoder.encode(&pcm, &mut payload).unwrap();
            payload.truncate(len);
            let packet = AudioPacket {
                seq: self.seq,
                timestamp: (self.frame * DEFAULT_FRAME) as u32,
                end_of_burst,
                payload,
            };
            self.seq = self.seq.wrapping_add(1);
            self.frame += 1;
            packet.encode()
        }
    }

    /// Mixes 20 ms at 48 kHz stereo and returns the mean square.
    fn play(audio: &StreamPlayer, speaking: bool) -> f32 {
        let mut out = vec![0.0; DEFAULT_FRAME * 2];
        audio.mix(&mut out, 2, RATE, speaking);
        out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32
    }

    #[test]
    fn plays_after_buffering_and_stops_at_end_of_burst() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        assert_eq!(play(&audio, false), 0.0);

        audio.push(&host.packet(false));
        assert_eq!(play(&audio, false), 0.0, "one packet is below the target");
        for _ in 0..5 {
            audio.push(&host.packet(false));
            play(&audio, false);
        }
        audio.push(&host.packet(false));
        assert!(play(&audio, false) > 0.05);

        audio.push(&host.packet(true));
        for _ in 0..4 {
            play(&audio, false);
        }
        assert_eq!(audio.inner.lock().unwrap().state, State::Idle);
        assert_eq!(play(&audio, false), 0.0);
    }

    #[test]
    fn late_packets_are_dropped_and_losses_concealed() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        let packets: Vec<Vec<u8>> = (0..8).map(|_| host.packet(false)).collect();
        audio.push(&packets[0]);
        audio.push(&packets[1]);
        audio.push(&packets[3]); // 2 is lost
        for _ in 0..3 {
            play(&audio, false);
        }
        let inner = audio.inner.lock().unwrap();
        assert_eq!(inner.state, State::Playing);
        let next = inner.next;
        drop(inner);
        audio.push(&packets[2]); // arrives after its time
        assert!(
            audio
                .inner
                .lock()
                .unwrap()
                .packets
                .keys()
                .all(|&seq| seq >= next)
        );
    }

    #[test]
    fn underrun_rebuffers_with_a_larger_target() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        audio.push(&host.packet(false));
        audio.push(&host.packet(false));
        for _ in 0..10 {
            play(&audio, false);
        }
        let inner = audio.inner.lock().unwrap();
        assert_eq!(inner.state, State::Buffering { fresh: false });
        assert_eq!(inner.target, MIN_TARGET + 1);
    }

    #[test]
    fn a_restarted_stream_plays_after_reset() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        host.seq = 3000;
        for _ in 0..10 {
            audio.push(&host.packet(false));
            play(&audio, false);
        }
        audio.reset();
        let mut host = Host::new(); // numbers from zero again
        for _ in 0..4 {
            audio.push(&host.packet(false));
        }
        assert!(play(&audio, false) > 0.05);
    }

    #[test]
    fn a_jump_in_numbering_starts_over() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        host.seq = 3000;
        for _ in 0..10 {
            audio.push(&host.packet(false));
            play(&audio, false);
        }
        // No reset: the new stream's packets arrive before its AudioStarted.
        let mut host = Host::new();
        for _ in 0..4 {
            audio.push(&host.packet(false));
        }
        assert!(play(&audio, false) > 0.05);
    }

    #[test]
    fn sequence_numbers_wrap() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        host.seq = u16::MAX - 1;
        for _ in 0..4 {
            audio.push(&host.packet(false));
        }
        let inner = audio.inner.lock().unwrap();
        let seqs: Vec<u64> = inner.packets.keys().copied().collect();
        assert_eq!(seqs.len(), 4);
        assert!(seqs.windows(2).all(|pair| pair[1] == pair[0] + 1));
    }

    #[test]
    fn ducking_lowers_host_audio_while_speech_plays() {
        let audio = StreamPlayer::new("test", 100, true);
        let mut host = Host::new();
        for _ in 0..3 {
            audio.push(&host.packet(false));
        }
        play(&audio, false);
        let mut normal = 0.0;
        let mut ducked = 0.0;
        for i in 0..20 {
            audio.push(&host.packet(false));
            if i >= 10 {
                ducked += play(&audio, true);
            } else {
                normal += play(&audio, false);
            }
        }
        assert!(ducked < normal * 0.3, "{ducked} vs {normal}");
    }

    #[test]
    fn clock_correction_follows_the_buffer_level() {
        let audio = StreamPlayer::new("test", 100, false);
        let mut host = Host::new();
        for _ in 0..3 {
            audio.push(&host.packet(false));
        }
        play(&audio, false);
        // The host runs fast: packets pile up, so playback speeds up.
        for _ in 0..8 {
            audio.push(&host.packet(false));
        }
        let mut inner = audio.inner.lock().unwrap();
        for _ in 0..200 {
            inner.clock_correction();
        }
        assert!(inner.clock_correction() > 0.0);
        assert!(inner.clock_correction() <= MAX_CORRECTION);
    }
}
