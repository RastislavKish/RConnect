//! Runs a session: speech from the host goes to the speech engine, playback events go
//! back to the host, keys from the user interface go to the host, the host's audio
//! stream goes to the output, and the microphone goes to the host.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rconnect_protocol::messages::{
    AudioMode, Capabilities, KeyMessage, MicMode, SpeechCommand, SpeechEvent,
};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::Instant;

use crate::mic::MicStream;
use crate::speech::{SpeechEngine, StreamPlayer, VoiceSettings};
use crate::{ConnectionPath, Session, SessionEnd, SessionOutput};

/// What the user interface sends to a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInput {
    Key(KeyMessage),
    /// Measure the round trip time to the host; the result goes to
    /// [`SessionObserver::ping`].
    Ping,
    /// Stream the host's audio in this mode, or stop with `None`. The outcome goes to
    /// [`SessionObserver::audio`].
    Audio(Option<AudioMode>),
    /// Set the microphone's mode. The outcome goes to [`SessionObserver::mic`].
    Mic(MicMode),
    /// End the session from the controller.
    End,
}

pub struct SpeechConfig {
    /// Directory containing `espeak-ng-data`.
    pub data_dir: PathBuf,
    /// The phone's voice, rate, pitch and volume.
    pub voice: VoiceSettings,
    /// Play inaudible noise between utterances to keep Bluetooth headphones awake.
    pub keep_alive: bool,
    /// The host's audio stream at the start of the session; `None` is off.
    pub audio_mode: Option<AudioMode>,
    /// Volume of the host's audio, 0..=100.
    pub audio_volume: u8,
    /// Turn the host's audio down while speech plays.
    pub audio_ducking: bool,
    /// The microphone at the start of the session.
    pub mic_mode: MicMode,
    /// Whether the app may use the microphone. Without permission, Unmuted and VAD
    /// behave as Muted: the host's microphone exists and stays silent.
    pub mic_permitted: bool,
    /// The microphone to capture from (Android's device id), or the default.
    pub mic_device: Option<i32>,
}

/// The host's virtual microphone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MicState {
    /// The mode the host confirmed.
    Mode(MicMode),
    /// The host cannot create a microphone.
    Unavailable,
}

/// Whether the host's audio streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioState {
    Off,
    Streaming(AudioMode),
    /// The host cannot stream audio.
    Unavailable,
}

/// Observes a running session, e.g. to show the spoken text on screen.
pub trait SessionObserver: Send + Sync {
    fn speech(&self, _text: &str) {}
    /// Round trip time of a [`SessionInput::Ping`], or `None` when the host did not answer
    /// within [`PING_TIMEOUT`], and the path the connection uses at that moment.
    fn ping(&self, _round_trip: Option<Duration>, _path: ConnectionPath) {}
    /// The host's audio stream started, changed mode or stopped.
    fn audio(&self, _state: AudioState) {}
    /// The host's microphone changed mode, or cannot be created.
    fn mic(&self, _state: MicState) {}
}

/// How long to wait for the host's answer to a ping.
pub const PING_TIMEOUT: Duration = Duration::from_secs(5);

/// Runs the session until it ends and returns why it ended.
pub async fn drive_session(
    mut session: Session,
    mut inputs: UnboundedReceiver<SessionInput>,
    speech: SpeechConfig,
    observer: &dyn SessionObserver,
) -> SessionEnd {
    let mut outputs = session
        .take_outputs()
        .expect("the session's outputs are unused");
    let host_audio = Arc::new(StreamPlayer::new(
        "host audio",
        speech.audio_volume,
        speech.audio_ducking,
    ));
    // Datagrams go straight to the jitter buffer, without passing through the loop.
    let datagrams = tokio::spawn({
        let connection = session.connection();
        let host_audio = host_audio.clone();
        async move {
            while let Ok(datagram) = connection.read_datagram().await {
                host_audio.push(&datagram);
            }
        }
    });
    let end = run(
        &mut session,
        &mut outputs,
        &mut inputs,
        speech,
        host_audio,
        observer,
    )
    .await;
    datagrams.abort();
    match end {
        Some(end) => end,
        None => {
            session.end().await;
            SessionEnd::ByController
        }
    }
}

/// The session loop. Returns `None` when the session is to be ended from this side.
async fn run(
    session: &mut Session,
    outputs: &mut UnboundedReceiver<SessionOutput>,
    inputs: &mut UnboundedReceiver<SessionInput>,
    speech: SpeechConfig,
    host_audio: Arc<StreamPlayer>,
    observer: &dyn SessionObserver,
) -> Option<SessionEnd> {
    let (engine, mut events) = match SpeechEngine::start(
        speech.data_dir,
        speech.voice,
        speech.keep_alive,
        host_audio.clone(),
    ) {
        Ok((engine, events)) => (Some(engine), Some(events)),
        Err(e) => {
            tracing::error!("speech is unavailable: {e:#}");
            (None, None)
        }
    };
    // Number and send time of the ping waiting for an answer. A new ping replaces it.
    let mut ping: Option<(u32, Instant)> = None;
    let mut next_ping = 0u32;
    let audio_available = session
        .host_capabilities
        .contains(Capabilities::AUDIO_TO_CONTROLLER);
    if speech.audio_mode.is_some() {
        if !audio_available {
            observer.audio(AudioState::Unavailable);
        } else if session.request_audio(speech.audio_mode).await.is_err() {
            return Some(SessionEnd::ConnectionLost);
        }
    }
    let mut mic = Mic {
        available: session
            .host_capabilities
            .contains(Capabilities::AUDIO_TO_HOST),
        permitted: speech.mic_permitted,
        device: speech.mic_device,
        stream: None,
    };
    if speech.mic_mode != MicMode::Off && mic.set(session, speech.mic_mode, observer).await.is_err()
    {
        return Some(SessionEnd::ConnectionLost);
    }

    loop {
        tokio::select! {
            output = outputs.recv() => match output {
                Some(SessionOutput::Speech(command)) => {
                    if let Some(event) = play(engine.as_ref(), command, observer)
                        && session.send_speech_event(event).await.is_err() {
                            return Some(SessionEnd::ConnectionLost);
                        }
                }
                Some(SessionOutput::Pong(number)) => {
                    if let Some((sent_number, sent)) = ping
                        && sent_number == number {
                            observer.ping(Some(sent.elapsed()), session.path());
                            ping = None;
                        }
                }
                Some(SessionOutput::AudioStarted(format)) => {
                    tracing::info!("host audio: {format:?}");
                    host_audio.reset();
                    observer.audio(AudioState::Streaming(format.mode));
                }
                Some(SessionOutput::AudioStopped) => {
                    host_audio.reset();
                    observer.audio(AudioState::Off);
                }
                Some(SessionOutput::MicState(mode)) => observer.mic(MicState::Mode(mode)),
                Some(SessionOutput::MicUnavailable) => {
                    mic.stream = None;
                    observer.mic(MicState::Unavailable);
                }
                Some(SessionOutput::Ended(end)) => return Some(end),
                None => return Some(SessionEnd::ConnectionLost),
            },
            Some(event) = recv(&mut events) => {
                if session.send_speech_event(event).await.is_err() {
                    return Some(SessionEnd::ConnectionLost);
                }
            }
            input = inputs.recv() => match input {
                Some(SessionInput::Key(key)) => {
                    if session.send_key(key).await.is_err() {
                        return Some(SessionEnd::ConnectionLost);
                    }
                }
                Some(SessionInput::Ping) => {
                    next_ping = next_ping.wrapping_add(1);
                    ping = Some((next_ping, Instant::now()));
                    if session.send_ping(next_ping).await.is_err() {
                        return Some(SessionEnd::ConnectionLost);
                    }
                }
                Some(SessionInput::Audio(mode)) => {
                    if !audio_available {
                        observer.audio(AudioState::Unavailable);
                    } else if session.request_audio(mode).await.is_err() {
                        return Some(SessionEnd::ConnectionLost);
                    }
                }
                Some(SessionInput::Mic(mode)) => {
                    if mic.set(session, mode, observer).await.is_err() {
                        return Some(SessionEnd::ConnectionLost);
                    }
                }
                Some(SessionInput::End) | None => return None,
            },
            _ = ping_expired(ping) => {
                observer.ping(None, session.path());
                ping = None;
            }
        }
    }
}

/// The phone's side of the microphone.
struct Mic {
    /// Whether the host offers a microphone.
    available: bool,
    permitted: bool,
    device: Option<i32>,
    /// Open while the microphone is unmuted and permitted.
    stream: Option<MicStream>,
}

impl Mic {
    /// Asks the host for `mode`, and opens or closes the phone's microphone to match.
    /// Fails only when the connection is lost.
    async fn set(
        &mut self,
        session: &mut Session,
        mode: MicMode,
        observer: &dyn SessionObserver,
    ) -> anyhow::Result<()> {
        if !self.available {
            observer.mic(MicState::Unavailable);
            return Ok(());
        }
        // Asked first, so the host is ready for the audio that follows.
        session.request_mic(mode).await?;
        let live = matches!(mode, MicMode::Unmuted | MicMode::Vad);
        if !live || !self.permitted {
            self.stream = None;
        } else if let Some(stream) = &self.stream {
            stream.set_mode(mode);
        } else {
            let connection = session.connection();
            let device = self.device;
            // Opening a microphone blocks for a moment; keep it off the session loop.
            match tokio::task::spawn_blocking(move || MicStream::start(connection, mode, device))
                .await
            {
                Ok(Ok(stream)) => self.stream = Some(stream),
                Ok(Err(e)) => tracing::warn!("cannot open the microphone: {e:#}"),
                Err(e) => tracing::warn!("cannot open the microphone: {e}"),
            }
        }
        Ok(())
    }
}

async fn ping_expired(ping: Option<(u32, Instant)>) {
    match ping {
        Some((_, sent)) => tokio::time::sleep_until(sent + PING_TIMEOUT).await,
        None => std::future::pending().await,
    }
}

/// Plays a speech command. Returns an event to send right away, if any.
fn play(
    engine: Option<&SpeechEngine>,
    command: SpeechCommand,
    observer: &dyn SessionObserver,
) -> Option<SpeechEvent> {
    match (engine, command) {
        (_, SpeechCommand::Speak(utterance)) if engine.is_none() => {
            // Without speech, finish at once so the host is not left waiting.
            observer.speech(&utterance.text);
            Some(SpeechEvent::End { id: utterance.id })
        }
        (Some(engine), SpeechCommand::Speak(utterance)) => {
            observer.speech(&utterance.text);
            engine.speak(utterance);
            None
        }
        (Some(engine), SpeechCommand::Stop { .. }) => {
            engine.stop();
            None
        }
        (engine, SpeechCommand::Pause { id }) => {
            let last_mark = engine.and_then(|engine| engine.pause(id));
            Some(SpeechEvent::Paused { id, last_mark })
        }
        (Some(engine), SpeechCommand::Settings(settings)) => {
            engine.set_settings(settings);
            None
        }
        (None, _) => None,
    }
}

async fn recv(events: &mut Option<UnboundedReceiver<SpeechEvent>>) -> Option<SpeechEvent> {
    match events {
        Some(events) => events.recv().await,
        None => std::future::pending().await,
    }
}
