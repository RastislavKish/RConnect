//! The phone's microphone: AAudio on Android, cpal elsewhere (development on Linux).
//!
//! The audio callback only converts samples and hands them to the encoder thread; it never
//! encodes or blocks. Chunks are dropped when the encoder falls behind.

use std::sync::mpsc::SyncSender;

pub use platform::Capture;

#[cfg(target_os = "android")]
mod platform {
    use super::*;
    use ndk::audio::{
        AudioCallbackResult, AudioDirection, AudioFormat, AudioInputPreset, AudioPerformanceMode,
        AudioSharingMode, AudioStream, AudioStreamBuilder,
    };

    pub struct Capture {
        stream: AudioStream,
    }

    // SAFETY: AAudio's stream functions may be called from any thread, except that a
    // stream must not be stopped or closed from its own callback. `Capture` only stops and
    // closes the stream when dropped, which never happens in the callback.
    unsafe impl Send for Capture {}

    impl Capture {
        /// Opens the microphone `device_id` (the phone's built-in one, chosen by the app), or
        /// the default. Returns the capture and its sample rate.
        pub fn open(
            device_id: Option<i32>,
            chunks: SyncSender<Vec<f32>>,
        ) -> anyhow::Result<(Capture, u32)> {
            let mut builder = AudioStreamBuilder::new()?
                .direction(AudioDirection::Input)
                .format(AudioFormat::PCM_I16)
                .channel_count(1)
                .performance_mode(AudioPerformanceMode::LowLatency)
                .sharing_mode(AudioSharingMode::Shared)
                // Clean audio without Android's call mode, which would change routing.
                .input_preset(AudioInputPreset::VoiceRecognition);
            if let Some(id) = device_id {
                builder = builder.device_id(id);
            }
            let stream = builder
                .data_callback(Box::new(move |_, data, frames| {
                    // SAFETY: AAudio passes `frames` mono PCM_I16 frames.
                    let samples =
                        unsafe { std::slice::from_raw_parts(data.cast::<i16>(), frames as usize) };
                    let _ = chunks.try_send(samples.iter().map(|&s| s as f32 / 32768.0).collect());
                    AudioCallbackResult::Continue
                }))
                .error_callback(Box::new(|_, error| {
                    tracing::warn!("microphone error: {error:?}");
                }))
                .open_stream()?;
            stream.request_start()?;
            let rate = stream.sample_rate() as u32;
            tracing::info!(
                "microphone: device {}, {rate} Hz, burst {} frames",
                stream.device_id(),
                stream.frames_per_burst()
            );
            Ok((Capture { stream }, rate))
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            let _ = self.stream.request_stop();
        }
    }
}

#[cfg(not(target_os = "android"))]
mod platform {
    use super::*;
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    pub enum Capture {
        /// Captures while it lives.
        #[allow(dead_code)]
        Device(cpal::Stream),
        /// `RCONNECT_DEV_MIC=tone` or `noise`, for tests without a microphone.
        Tone(Arc<AtomicBool>),
    }

    impl Capture {
        pub fn open(
            _device_id: Option<i32>,
            chunks: SyncSender<Vec<f32>>,
        ) -> anyhow::Result<(Capture, u32)> {
            match std::env::var("RCONNECT_DEV_MIC").as_deref() {
                Ok("tone") => return Ok((Capture::tone(chunks, false), 48_000)),
                Ok("noise") => return Ok((Capture::tone(chunks, true), 48_000)),
                _ => {}
            }
            let device = cpal::default_host()
                .default_input_device()
                .ok_or_else(|| anyhow::anyhow!("no microphone"))?;
            let config = device.default_input_config()?;
            let channels = config.channels() as usize;
            let rate = config.sample_rate();
            let stream = device.build_input_stream(
                config.config(),
                move |data: &[f32], _| {
                    let mono = data
                        .chunks_exact(channels)
                        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                        .collect();
                    let _ = chunks.try_send(mono);
                },
                |error| tracing::warn!("microphone error: {error}"),
                None,
            )?;
            stream.play()?;
            Ok((Capture::Device(stream), rate))
        }

        /// Generates `RCONNECT_DEV_MIC=tone` (440 Hz) or `noise` in real time, paced by the
        /// clock rather than by sleeping, which would fall behind.
        fn tone(chunks: SyncSender<Vec<f32>>, noise: bool) -> Capture {
            let running = Arc::new(AtomicBool::new(true));
            let flag = running.clone();
            std::thread::spawn(move || {
                let start = std::time::Instant::now();
                let mut n = 0u64;
                let mut seed = 0x1234_5678u32;
                while flag.load(Ordering::Relaxed) {
                    let due = start.elapsed().as_micros() as u64 * 48 / 1000;
                    if due > n {
                        let chunk = (n..due)
                            .map(|i| {
                                if noise {
                                    seed ^= seed << 13;
                                    seed ^= seed >> 17;
                                    seed ^= seed << 5;
                                    0.3 * (seed as f32 / u32::MAX as f32 * 2.0 - 1.0)
                                } else {
                                    0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0)
                                        .sin()
                                }
                            })
                            .collect();
                        n = due;
                        let _ = chunks.try_send(chunk);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            });
            Capture::Tone(running)
        }
    }

    impl Drop for Capture {
        fn drop(&mut self) {
            if let Capture::Tone(running) = self {
                running.store(false, Ordering::Relaxed);
            }
        }
    }
}
