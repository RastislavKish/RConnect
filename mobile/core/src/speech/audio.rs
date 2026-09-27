//! Audio output: one stream, kept open for the whole session, pulling from [`Playback`].
//!
//! On Android this is AAudio in low latency mode with the accessibility usage, so speech
//! follows the accessibility volume like TalkBack. Elsewhere (development on Linux) it is
//! cpal's default output device.

use std::sync::{Arc, Mutex};

use super::playback::Playback;

pub use platform::Output;

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use ndk::audio::{
        AudioCallbackResult, AudioContentType, AudioDirection, AudioFormat, AudioPerformanceMode,
        AudioSharingMode, AudioStream, AudioStreamBuilder, AudioUsage,
    };
    use std::sync::atomic::{AtomicBool, Ordering};

    use crate::speech::playback::Resampler;

    pub struct Output {
        stream: AudioStream,
        /// Set by AAudio when the stream breaks, e.g. when headphones are connected.
        broken: Arc<AtomicBool>,
    }

    impl Output {
        pub fn open(sample_rate: u32, playback: Arc<Mutex<Playback>>) -> anyhow::Result<Output> {
            let broken = Arc::new(AtomicBool::new(false));
            let broken_flag = broken.clone();
            let mut resampler: Option<Resampler> = None;
            let stream = AudioStreamBuilder::new()?
                .direction(AudioDirection::Output)
                .format(AudioFormat::PCM_I16)
                .channel_count(1)
                .sample_rate(sample_rate as i32)
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
                    let Ok(mut playback) = playback.lock() else {
                        out.fill(0);
                        return AudioCallbackResult::Continue;
                    };
                    let device_rate = stream.sample_rate() as u32;
                    if device_rate == sample_rate && channels == 1 {
                        playback.fill(out);
                    } else {
                        resampler
                            .get_or_insert_with(|| {
                                Resampler::new(sample_rate, device_rate, channels)
                            })
                            .fill(&mut playback, out, |v| (v * 32767.0) as i16);
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

    use crate::speech::playback::Resampler;

    pub struct Output {
        _stream: cpal::Stream,
        broken: Arc<AtomicBool>,
    }

    impl Output {
        pub fn open(sample_rate: u32, playback: Arc<Mutex<Playback>>) -> anyhow::Result<Output> {
            let device = cpal::default_host()
                .default_output_device()
                .ok_or_else(|| anyhow::anyhow!("no audio output device"))?;
            let config = device.default_output_config()?;
            let channels = config.channels() as usize;
            let device_rate = config.sample_rate();
            let broken = Arc::new(AtomicBool::new(false));
            let broken_flag = broken.clone();
            let mut resampler = Resampler::new(sample_rate, device_rate, channels);

            let stream = device.build_output_stream(
                config.config(),
                move |out: &mut [f32], _| match playback.lock() {
                    Ok(mut playback) => resampler.fill(&mut playback, out, |v| v),
                    Err(_) => out.fill(0.0),
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
