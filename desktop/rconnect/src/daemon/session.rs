//! Remote control sessions on [`ALPN_SESSION`].
//!
//! [`ALPN_SESSION`]: rconnect_protocol::ALPN_SESSION

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use rconnect_protocol::messages::{
    AudioDirection, AudioRequest, Capabilities, ControlMessage, EndReason, Handshake, KeyMessage,
    RejectReason, SpeechCommand, SpeechEvent,
};
use rconnect_protocol::{PROTOCOL_VERSION, framing};
use tokio::io::BufReader;
use tokio::sync::{mpsc, oneshot};

use super::Daemon;
use super::audio::{AudioStream, CaptureKind};
use super::inhibit::SleepInhibitor;
use crate::speech::ipc::OutputMode;
use crate::store::AudioCapture;

/// Priority of the speech stream; keys arrive on a stream the phone sends, where the phone
/// sets its own priority.
const SPEECH_PRIORITY: i32 = 10;

/// The active session, as seen by the rest of the daemon.
pub struct SessionHandle {
    token: u64,
    controller: EndpointId,
    pub name: String,
    end: Option<oneshot::Sender<EndReason>>,
}

/// Why a session loop stopped.
enum Ended {
    /// Ended on the host (replaced, revoked, shutdown); the phone still has to be told.
    ByHost(EndReason),
    /// The phone ended it or went away.
    ByController(&'static str),
}

impl Daemon {
    pub(super) async fn run_session(self: Arc<Self>, connection: Connection) -> anyhow::Result<()> {
        let controller = connection.remote_id();
        let (mut control_send, control_recv) = connection.accept_bi().await?;
        let mut control_recv = BufReader::new(control_recv);

        let hello = tokio::time::timeout(
            Duration::from_secs(10),
            framing::read_message::<_, ControlMessage>(&mut control_recv),
        )
        .await
        .context("no hello from the controller")??;
        let Some(ControlMessage::Hello(hello)) = hello else {
            anyhow::bail!("the controller did not start with Hello");
        };

        let name = self
            .devices
            .lock()
            .unwrap()
            .find(&controller)
            .map(|device| device.name.clone());
        let rejection = match &name {
            None => Some(RejectReason::NotPaired),
            Some(_) if hello.protocol_version != PROTOCOL_VERSION => {
                Some(RejectReason::UnsupportedVersion(PROTOCOL_VERSION))
            }
            Some(_) => None,
        };
        let registered = match (rejection, name) {
            (Some(reason), _) => Err(reason),
            (None, Some(name)) => self.begin_session(controller, name),
            (None, None) => unreachable!(),
        };
        let (token, name, end_rx) = match registered {
            Ok(registered) => registered,
            Err(reason) => {
                tracing::info!("rejecting session from {controller}: {reason:?}");
                framing::write_message(&mut control_send, &ControlMessage::Reject(reason)).await?;
                control_send.finish()?;
                let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
                return Ok(());
            }
        };

        tracing::info!("session started with {name:?} ({})", hello.app_version);
        let ended = self
            .clone()
            .session_loop(&connection, token, control_send, control_recv, end_rx)
            .await;

        self.keyboard.lock().unwrap().release_all();
        self.speech.detach(token);
        self.finish_session(token);
        match ended {
            Ok(Ended::ByHost(reason)) => {
                tracing::info!("session with {name:?} ended: {reason:?}");
            }
            Ok(Ended::ByController(why)) => tracing::info!("session with {name:?} ended: {why}"),
            Err(e) => tracing::info!("session with {name:?} ended: {e:#}"),
        }
        connection.close(0u32.into(), b"session ended");
        Ok(())
    }

    async fn session_loop(
        self: Arc<Self>,
        connection: &Connection,
        token: u64,
        mut control_send: SendStream,
        mut control_recv: BufReader<RecvStream>,
        end_rx: oneshot::Receiver<EndReason>,
    ) -> anyhow::Result<Ended> {
        let capabilities = if self.config.audio == AudioCapture::Off {
            Capabilities::NONE
        } else {
            Capabilities::AUDIO_TO_CONTROLLER
        };
        let welcome = ControlMessage::Welcome(Handshake {
            protocol_version: PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").into(),
            device_name: self.config.name.clone(),
            capabilities,
        });
        framing::write_message(&mut control_send, &welcome).await?;

        let (speech_send, speech_recv) = connection.open_bi().await?;
        speech_send.set_priority(SPEECH_PRIORITY)?;
        let (speech_tx, speech_rx) = mpsc::unbounded_channel();
        self.speech
            .attach(token, speech_tx, self.speech_mode(false));

        self.keyboard.lock().unwrap().ensure_device();
        let _inhibitor = match SleepInhibitor::acquire().await {
            Ok(inhibitor) => Some(inhibitor),
            Err(e) => {
                tracing::warn!("cannot prevent sleep during the session: {e}");
                None
            }
        };

        let ended = tokio::select! {
            result = self.control_loop(connection, token, &mut control_recv, &mut control_send) => result?,
            result = self.clone().keys_loop(connection) => result?,
            result = speech_out(speech_rx, speech_send) => result?,
            result = self.clone().speech_in(speech_recv) => result?,
            reason = end_rx => Ended::ByHost(reason.unwrap_or(EndReason::Error)),
            _ = connection.closed() => Ended::ByController("connection lost"),
        };

        if let Ended::ByHost(reason) = ended {
            let _ = framing::write_message(&mut control_send, &ControlMessage::EndSession(reason))
                .await;
            let _ = control_send.finish();
            let _ = tokio::time::timeout(Duration::from_secs(1), connection.closed()).await;
        }
        Ok(ended)
    }

    async fn keys_loop(self: Arc<Self>, connection: &Connection) -> anyhow::Result<Ended> {
        let mut keys = BufReader::new(connection.accept_uni().await?);
        loop {
            match framing::read_message::<_, KeyMessage>(&mut keys).await {
                Ok(Some(message)) => {
                    tracing::trace!("key {message:?}");
                    self.keyboard.lock().unwrap().apply(message);
                }
                Ok(None) => return Ok(Ended::ByController("keys stream closed")),
                Err(e) if e.is_recoverable() => tracing::warn!("ignoring key message: {e}"),
                Err(e) => return Err(e.into()),
            }
        }
    }

    async fn speech_in(self: Arc<Self>, speech: RecvStream) -> anyhow::Result<Ended> {
        let mut speech = BufReader::new(speech);
        loop {
            match framing::read_message::<_, SpeechEvent>(&mut speech).await {
                Ok(Some(event)) => self.speech.event(event),
                Ok(None) => return Ok(Ended::ByController("speech stream closed")),
                Err(e) if e.is_recoverable() => tracing::warn!("ignoring speech event: {e}"),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Registers a new session. A session from the same controller replaces the existing
    /// one (the phone reconnected); another controller is refused.
    fn begin_session(
        &self,
        controller: EndpointId,
        name: String,
    ) -> Result<(u64, String, oneshot::Receiver<EndReason>), RejectReason> {
        let mut session = self.session.lock().unwrap();
        if let Some(existing) = session.as_mut() {
            if existing.controller != controller {
                return Err(RejectReason::Busy);
            }
            if let Some(end) = existing.end.take() {
                let _ = end.send(EndReason::Replaced);
            }
        }
        let token = self.next_session_token.fetch_add(1, Ordering::Relaxed);
        let (end, end_rx) = oneshot::channel();
        *session = Some(SessionHandle {
            token,
            controller,
            name: name.clone(),
            end: Some(end),
        });
        Ok((token, name, end_rx))
    }

    fn finish_session(&self, token: u64) {
        let mut session = self.session.lock().unwrap();
        if session
            .as_ref()
            .is_some_and(|session| session.token == token)
        {
            *session = None;
        }
    }

    /// Ends the active session if its controller matches.
    pub(super) fn end_session_if(&self, matches: impl Fn(&EndpointId) -> bool, reason: EndReason) {
        let mut session = self.session.lock().unwrap();
        if let Some(session) = session.as_mut()
            && matches(&session.controller)
            && let Some(end) = session.end.take()
        {
            let _ = end.send(reason);
        }
    }
}

impl Daemon {
    /// Where speech plays. Capturing the sink's monitor would repeat local speech on the
    /// phone, so it is kept off the laptop while the monitor is captured.
    fn speech_mode(&self, capturing_monitor: bool) -> OutputMode {
        if self.config.local_speech && !capturing_monitor {
            OutputMode::Mirror
        } else {
            OutputMode::Remote
        }
    }

    async fn control_loop(
        &self,
        connection: &Connection,
        token: u64,
        control: &mut BufReader<RecvStream>,
        replies: &mut SendStream,
    ) -> anyhow::Result<Ended> {
        let mut audio: Option<AudioStream> = None;
        loop {
            match framing::read_message::<_, ControlMessage>(control).await {
                Ok(Some(ControlMessage::Ping(number))) => {
                    framing::write_message(replies, &ControlMessage::Pong(number)).await?;
                }
                Ok(Some(ControlMessage::EndSession(_))) => {
                    return Ok(Ended::ByController("ended on the phone"));
                }
                Ok(Some(ControlMessage::AudioRequest(request))) => {
                    let reply = self.start_audio(connection, token, &mut audio, request);
                    framing::write_message(replies, &reply).await?;
                }
                Ok(Some(ControlMessage::AudioStop)) => {
                    if audio.take().is_some() {
                        self.speech.set_mode(token, self.speech_mode(false));
                    }
                    framing::write_message(replies, &ControlMessage::AudioStop).await?;
                }
                Ok(Some(other)) => tracing::warn!("unexpected control message {other:?}"),
                Ok(None) => return Ok(Ended::ByController("control stream closed")),
                Err(e) if e.is_recoverable() => tracing::warn!("ignoring control message: {e}"),
                Err(e) => return Err(e.into()),
            }
        }
    }

    /// Starts the audio stream or changes its mode, and returns the answer for the phone.
    fn start_audio(
        &self,
        connection: &Connection,
        token: u64,
        audio: &mut Option<AudioStream>,
        request: AudioRequest,
    ) -> ControlMessage {
        let kind = match self.config.audio {
            AudioCapture::Streams => CaptureKind::Streams,
            AudioCapture::Monitor => CaptureKind::Monitor,
            AudioCapture::Off => return ControlMessage::AudioStop,
        };
        if request.direction != AudioDirection::ToController {
            return ControlMessage::AudioStop;
        }
        if let Some(stream) = audio {
            stream.set_mode(request.mode);
            return ControlMessage::AudioStarted(AudioStream::format(request.mode));
        }
        // Connecting to PipeWire takes a moment; don't hold up other sessions' tasks.
        let started = tokio::task::block_in_place(|| {
            AudioStream::start(connection.clone(), request.mode, kind)
        });
        match started {
            Ok(stream) => {
                if kind == CaptureKind::Monitor {
                    self.speech.set_mode(token, self.speech_mode(true));
                }
                *audio = Some(stream);
                ControlMessage::AudioStarted(AudioStream::format(request.mode))
            }
            Err(e) => {
                tracing::warn!("cannot stream audio: {e:#}");
                ControlMessage::AudioStop
            }
        }
    }
}

async fn speech_out(
    mut commands: mpsc::UnboundedReceiver<SpeechCommand>,
    mut stream: SendStream,
) -> anyhow::Result<Ended> {
    while let Some(command) = commands.recv().await {
        framing::write_message(&mut stream, &command).await?;
    }
    // The speech hub dropped this session's sink because a newer session attached. The
    // session is ended through its end channel, so just wait for that.
    std::future::pending().await
}
