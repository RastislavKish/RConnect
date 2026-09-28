//! Audio output: one stream, kept open for the whole session, pulling from [`Playback`]
//! and mixing in [`HostAudio`].
//!
//! On Android this is AAudio in low latency mode with the accessibility usage, so speech
//! follows the accessibility volume like TalkBack. The stream runs at the device's own
//! rate in stereo: no rate is requested, because a rate the device does not use natively
//! can cost the low latency path. Elsewhere (development on Linux) it is cpal's default
//! output device.

use std::sync::{Arc, Mutex};

use super::host_audio::HostAudio;
use super::playback::{Playback, Resampler};

pub use platform::Output;

/// Renders one buffer: speech resampled to the device's rate, host audio on top.
struct Mixer {
    speech_rate: u32,
    playback: Arc<Mutex<Playback>>,
    host: Arc<HostAudio>,
    /// Created for the device's rate and channels, recreated if they change.
    resampler: Option<(u32, usize, Resampler)>,
    buffer: Vec<f32>,
}

impl Mixer {
    fn new(speech_rate: u32, playback: Arc<Mutex<Playback>>, host: Arc<HostAudio>) -> Mixer {
        Mixer {
            speech_rate,
            playback,
            host,
            resampler: None,
            buffer: Vec::new(),
        }
    }

    /// Fills `buffer` with `frames` interleaved frames and returns it.
    fn render(&mut self, frames: usize, channels: usize, device_rate: u32) -> &[f32] {
        // Grows only while the device's buffer size grows, so rarely allocates.
        self.buffer.resize(frames * channels, 0.0);
        let resampler = match &mut self.resampler {
            Some((rate, count, resampler)) if *rate == device_rate && *count == channels => {
                resampler
            }
            slot => {
                let resampler = Resampler::new(self.speech_rate, device_rate, channels);
                &mut slot.insert((device_rate, channels, resampler)).2
            }
        };
        let speaking = match self.playback.lock() {
            Ok(mut playback) => {
                let speaking = playback.is_speaking();
                resampler.fill(&mut playback, &mut self.buffer, |v| v);
                speaking
            }
            Err(_) => {
                self.buffer.fill(0.0);
                false
            }
        };
        self.host
            .mix(&mut self.buffer, channels, device_rate, speaking);
        &self.buffer
    }
}

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use ndk::audio::{
        AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode,
        AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    pub struct Output {
        stream: AudioStream,
        /// Set by AAudio when the stream breaks, e.g. when headphones are connected.
        broken: Arc<AtomicBool>,
    }

    impl Output {
        pub fn open(
            sample_rate: u32,
            playback: Arc<Mutex<Playback>>,
            host: Arc<HostAudio>,
        ) -> anyhow::Result<Output> {
            let broken = Arc::new(AtomicBool::new(false));
            let broken_flag = broken.clone();
            let mut mixer = Mixer::new(sample_rate, playback, host);
            let stream = AudioStreamBuilder::new()?
                .direction(AudioDirection::Output)
                .format(AudioFormat::PCM_I16)
                .channel_count(2)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                .usage(AudioUsage::AssistanceAccessibility)
                .content_type(AudioContentType::Speech)
                .data_callback(Box::new(move |stream, data, frames| {
                    let channels = stream.channel_count().max(1) as usize;
                    // SAFETY: AAudio passes a buffer of `frames` frames in the stream's
                    // format, which is PCM_I16.
                    let out = unsafe {
                        std::slice::from_raw_parts_mut(
                            data.cast::<i16>(),
                            frames as usize * channels,
                        )
                    };
                    let mixed =
                        mixer.render(frames as usize, channels, stream.sample_rate() as u32);
                    for (slot, sample) in out.iter_mut().zip(mixed) {
                        *slot = (sample.clamp(-1.0, 1.0) * 32767.0) as i16;
                    }
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(move |_, error| {
                    tracing::warn!("audio stream error: {error:?}");
                    broken_flag.store(true, Ordering::Relaxed);
                }))
                .open_stream()?;
            stream.request_start()?;
            tracing::info!(
                "audio: {} Hz, {} channels, burst {} frames",
                stream.sample_rate(),
                stream.channel_count(),
                stream.frames_per_burst()
            );
            Ok(Output { stream, broken })
        }

        /// Whether the stream has to be reopened.
        pub fn is_broken(&self) -> bool {
            self.broken.load(Ordering::Relaxed)
        }
    }

    impl Drop for Output {
        fn drop(&mut self) {
            let _ = self.stream.request_stop();
        }
    }
}

#[cfg(not(target_os = "android"))]
mod platform {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::atomic::{AtomicBool, Ordering};

    pub struct Output {
        _stream: cpal::Stream,
        broken: Arc<AtomicBool>,
    }

    impl Output {
        pub fn open(
            sample_rate: u32,
            playback: Arc<Mutex<Playback>>,
            host: Arc<HostAudio>,
        ) -> anyhow::Result<Output> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
            let config = device.default_output_config()?;
            let channels = config.channels() as usize;
            let device_rate = config.sample_rate();
            let broken = Arc::new(AtomicBool::new(false));
            let broken_flag = broken.clone();
            let mut mixer = Mixer::new(sample_rate, playback, host);

            let stream = device.build_output_stream(
                config.config(),
                move |out: &mut [f32], _| {
                    let mixed = mixer.render(out.len() / channels, channels, device_rate);
                    for (slot, sample) in out.iter_mut().zip(mixed) {
                        *slot = sample.clamp(-1.0, 1.0);
                    }
                },
                move |error| {
                    tracing::warn!("audio stream error: {error}");
                    broken_flag.store(true, Ordering::Relaxed);
                },
                None,
            )?;
            stream.play()?;
            Ok(Output {
                _stream: stream,
                broken,
            })
        }

        pub fn is_broken(&self) -> bool {
            self.broken.load(Ordering::Relaxed)
        }
    }
}
