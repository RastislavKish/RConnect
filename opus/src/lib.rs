//! A small safe wrapper over libopus: the encoder and decoder RConnect's audio stream
//! needs, driven through a handful of hand-written FFI declarations.
//!
//! Audio is interleaved `f32` samples. Frames must be 2.5, 5, 10, 20, 40 or 60 ms long.

use std::os::raw::{c_int, c_uchar};

#[repr(C)]
struct OpusEncoder {
    _private: [u8; 0],
}

#[repr(C)]
struct OpusDecoder {
    _private: [u8; 0],
}

const OPUS_OK: c_int = 0;
const OPUS_APPLICATION_VOIP: c_int = 2048;
const OPUS_APPLICATION_RESTRICTED_LOWDELAY: c_int = 2051;
const OPUS_SET_BITRATE_REQUEST: c_int = 4002;
const OPUS_SET_VBR_REQUEST: c_int = 4006;
const OPUS_SET_COMPLEXITY_REQUEST: c_int = 4010;
const OPUS_SET_INBAND_FEC_REQUEST: c_int = 4012;
const OPUS_SET_PACKET_LOSS_PERC_REQUEST: c_int = 4014;
const OPUS_RESET_STATE: c_int = 4028;

unsafe extern "C" {
    fn opus_encoder_create(
        fs: i32,
        channels: c_int,
        application: c_int,
        error: *mut c_int,
    ) -> *mut OpusEncoder;
    fn opus_encode_float(
        st: *mut OpusEncoder,
        pcm: *const f32,
        frame_size: c_int,
        data: *mut c_uchar,
        max_data_bytes: i32,
    ) -> i32;
    fn opus_encoder_ctl(st: *mut OpusEncoder, request: c_int, ...) -> c_int;
    fn opus_encoder_destroy(st: *mut OpusEncoder);

    fn opus_decoder_create(fs: i32, channels: c_int, error: *mut c_int) -> *mut OpusDecoder;
    fn opus_decode_float(
        st: *mut OpusDecoder,
        data: *const c_uchar,
        len: i32,
        pcm: *mut f32,
        frame_size: c_int,
        decode_fec: c_int,
    ) -> c_int;
    fn opus_decoder_ctl(st: *mut OpusDecoder, request: c_int, ...) -> c_int;
    fn opus_decoder_destroy(st: *mut OpusDecoder);
}

/// A libopus error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Error(pub i32);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "opus error {}", self.0)
    }
}

impl std::error::Error for Error {}

fn check(code: c_int) -> Result<c_int, Error> {
    if code < OPUS_OK {
        Err(Error(code))
    } else {
        Ok(code)
    }
}

/// Largest packet the encoder produces; more than enough for one frame at our bitrates.
pub const MAX_PACKET: usize = 1275;

/// Opus encoder: [`Encoder::new`] for general audio, [`Encoder::voice`] for speech.
pub struct Encoder {
    state: *mut OpusEncoder,
    channels: usize,
}

// SAFETY: the encoder state is only reached through `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Restricted low-delay mode (CELT only, about 5 ms algorithmic delay), for any audio.
    pub fn new(sample_rate: u32, channels: u8, bitrate: u32) -> Result<Encoder, Error> {
        Encoder::create(
            sample_rate,
            channels,
            bitrate,
            OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        )
    }

    /// VoIP mode, tuned for speech, with in-band forward error correction: each packet
    /// also carries a coarse copy of the previous one, which [`Decoder::recover`] uses
    /// when that one is lost. `expected_loss` is in percent.
    pub fn voice(
        sample_rate: u32,
        channels: u8,
        bitrate: u32,
        expected_loss: u8,
    ) -> Result<Encoder, Error> {
        let encoder = Encoder::create(sample_rate, channels, bitrate, OPUS_APPLICATION_VOIP)?;
        // SAFETY: valid state; these requests take one int argument.
        unsafe {
            check(opus_encoder_ctl(
                encoder.state,
                OPUS_SET_INBAND_FEC_REQUEST,
                1 as c_int,
            ))?;
            check(opus_encoder_ctl(
                encoder.state,
                OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                expected_loss as c_int,
            ))?;
        }
        Ok(encoder)
    }

    fn create(
        sample_rate: u32,
        channels: u8,
        bitrate: u32,
        application: c_int,
    ) -> Result<Encoder, Error> {
        let mut error = 0;
        // SAFETY: plain constructor; the result is checked.
        let state = unsafe {
            opus_encoder_create(
                sample_rate as i32,
                channels as c_int,
                application,
                &mut error,
            )
        };
        check(error)?;
        let mut encoder = Encoder {
            state,
            channels: channels as usize,
        };
        encoder.set_bitrate(bitrate)?;
        // SAFETY: valid state; these requests take one int argument.
        unsafe {
            check(opus_encoder_ctl(
                encoder.state,
                OPUS_SET_VBR_REQUEST,
                1 as c_int,
            ))?;
            check(opus_encoder_ctl(
                encoder.state,
                OPUS_SET_COMPLEXITY_REQUEST,
                5 as c_int,
            ))?;
        }
        Ok(encoder)
    }

    pub fn set_bitrate(&mut self, bitrate: u32) -> Result<(), Error> {
        // SAFETY: valid state; the request takes one int argument.
        check(unsafe { opus_encoder_ctl(self.state, OPUS_SET_BITRATE_REQUEST, bitrate as c_int) })?;
        Ok(())
    }

    /// Encodes one frame of interleaved samples into `out`, returning the packet length.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, Error> {
        let frame_size = pcm.len() / self.channels;
        // SAFETY: `pcm` holds `frame_size` frames and `out` is writable for its length.
        let len = unsafe {
            opus_encode_float(
                self.state,
                pcm.as_ptr(),
                frame_size as c_int,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        Ok(check(len)? as usize)
    }

    /// Forgets the previous audio, e.g. before the first frame of a new burst.
    pub fn reset(&mut self) {
        // SAFETY: valid state; the request takes no argument.
        unsafe { opus_encoder_ctl(self.state, OPUS_RESET_STATE) };
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_encoder_create` and not used afterwards.
        unsafe { opus_encoder_destroy(self.state) };
    }
}

pub struct Decoder {
    state: *mut OpusDecoder,
    channels: usize,
}

// SAFETY: the decoder state is only reached through `&mut self`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(sample_rate: u32, channels: u8) -> Result<Decoder, Error> {
        let mut error = 0;
        // SAFETY: plain constructor; the result is checked.
        let state =
            unsafe { opus_decoder_create(sample_rate as i32, channels as c_int, &mut error) };
        check(error)?;
        Ok(Decoder {
            state,
            channels: channels as usize,
        })
    }

    /// Decodes a packet into `out`, returning the number of frames. `None` conceals a
    /// lost packet; `out` then decides how much audio is made up.
    pub fn decode(&mut self, packet: Option<&[u8]>, out: &mut [f32]) -> Result<usize, Error> {
        let (data, len) = match packet {
            Some(packet) => (packet.as_ptr(), packet.len() as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: `data` is valid for `len` bytes (or null for concealment) and `out` is
        // writable for `frame_size` frames.
        let frames = unsafe {
            opus_decode_float(
                self.state,
                data,
                len,
                out.as_mut_ptr(),
                (out.len() / self.channels) as c_int,
                0,
            )
        };
        Ok(check(frames)? as usize)
    }

    /// Recovers a lost packet from the forward error correction data in the packet that
    /// followed it. `out` holds exactly the lost packet's frames. Without such data, as
    /// in packets from [`Encoder::new`], this conceals the loss instead.
    pub fn recover(&mut self, next: &[u8], out: &mut [f32]) -> Result<usize, Error> {
        // SAFETY: `next` is valid for its length and `out` is writable for `frame_size`
        // frames.
        let frames = unsafe {
            opus_decode_float(
                self.state,
                next.as_ptr(),
                next.len() as i32,
                out.as_mut_ptr(),
                (out.len() / self.channels) as c_int,
                1,
            )
        };
        Ok(check(frames)? as usize)
    }

    /// Forgets the previous audio, e.g. at the start of a new burst.
    pub fn reset(&mut self) {
        // SAFETY: valid state; the request takes no argument.
        unsafe { opus_decoder_ctl(self.state, OPUS_RESET_STATE) };
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by `opus_decoder_create` and not used afterwards.
        unsafe { opus_decoder_destroy(self.state) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_survives_a_round_trip() {
        const RATE: u32 = 48_000;
        const FRAME: usize = 960;
        let mut encoder = Encoder::new(RATE, 2, 64_000).unwrap();
        let mut decoder = Decoder::new(RATE, 2).unwrap();
        let mut packet = [0u8; MAX_PACKET];
        let mut decoded = vec![0f32; FRAME * 2];
        let mut energy = 0.0;
        for frame in 0..20 {
            let pcm: Vec<f32> = (0..FRAME)
                .flat_map(|i| {
                    let t = (frame * FRAME + i) as f32 / RATE as f32;
                    let v = 0.5 * (2.0 * std::f32::consts::PI * 440.0 * t).sin();
                    [v, v]
                })
                .collect();
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            assert!(len > 0 && len < 400, "{len} bytes");
            let frames = decoder.decode(Some(&packet[..len]), &mut decoded).unwrap();
            assert_eq!(frames, FRAME);
            if frame > 5 {
                energy += decoded.iter().map(|s| s * s).sum::<f32>() / decoded.len() as f32;
            }
        }
        // A 0.5 amplitude sine has a mean square of 0.125.
        let mean = energy / 14.0;
        assert!((0.1..0.15).contains(&mean), "{mean}");

        let frames = decoder.decode(None, &mut decoded).unwrap();
        assert_eq!(frames, FRAME);
    }

    #[test]
    fn voice_frames_recover_a_lost_packet() {
        const RATE: u32 = 48_000;
        const FRAME: usize = 960;
        let mut encoder = Encoder::voice(RATE, 1, 24_000, 5).unwrap();
        let mut decoder = Decoder::new(RATE, 1).unwrap();
        let mut packets = Vec::new();
        for frame in 0..30 {
            // A vowel-like mix of harmonics, which SILK encodes as speech.
            let pcm: Vec<f32> = (0..FRAME)
                .map(|i| {
                    let t = (frame * FRAME + i) as f32 / RATE as f32;
                    (1..6)
                        .map(|h| 0.1 * (2.0 * std::f32::consts::PI * 150.0 * h as f32 * t).sin())
                        .sum()
                })
                .collect();
            let mut packet = [0u8; MAX_PACKET];
            let len = encoder.encode(&pcm, &mut packet).unwrap();
            packets.push(packet[..len].to_vec());
        }
        let mut out = vec![0f32; FRAME];
        for packet in &packets[..20] {
            decoder.decode(Some(packet), &mut out).unwrap();
        }
        // Packet 20 is lost; 21 carries it.
        let frames = decoder.recover(&packets[21], &mut out).unwrap();
        assert_eq!(frames, FRAME);
        let energy = out.iter().map(|s| s * s).sum::<f32>() / FRAME as f32;
        assert!(energy > 0.001, "recovered audio is silent: {energy}");
        decoder.decode(Some(&packets[21]), &mut out).unwrap();
    }

    #[test]
    fn silence_is_cheap() {
        let mut encoder = Encoder::new(48_000, 2, 64_000).unwrap();
        let mut packet = [0u8; MAX_PACKET];
        let silence = vec![0f32; 960 * 2];
        let mut len = 0;
        for _ in 0..10 {
            len = encoder.encode(&silence, &mut packet).unwrap();
        }
        assert!(len < 10, "{len} bytes");
    }
}
