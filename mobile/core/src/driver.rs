//! Runs a session: speech from the host goes to the speech engine, playback events go
//! back to the host, and keys from the user interface go to the host.

use std::path::PathBuf;

use rconnect_protocol::messages::{KeyMessage, SpeechCommand, SpeechEvent};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::speech::{SpeechEngine, VoiceSettings};
use crate::{Session, SessionEnd, SessionOutput};

/// What the user interface sends to a running session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInput {
    Key(KeyMessage),
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
}

/// Observes a running session, e.g. to show the spoken text on screen.
pub trait SessionObserver: Send + Sync {
    fn speech(&self, _text: &str) {}
}

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
    let (engine, mut events) =
        match SpeechEngine::start(speech.data_dir, speech.voice, speech.keep_alive) {
            Ok((engine, events)) => (Some(engine), Some(events)),
            Err(e) => {
                tracing::error!("speech is unavailable: {e:#}");
                (None, None)
            }
        };

    loop {
        tokio::select! {
            output = outputs.recv() => match output {
                Some(SessionOutput::Speech(command)) => {
                    if let Some(event) = play(engine.as_ref(), command, observer)
                        && session.send_speech_event(event).await.is_err() {
                            return SessionEnd::ConnectionLost;
                        }
                }
                Some(SessionOutput::Ended(end)) => return end,
                None => return SessionEnd::ConnectionLost,
            },
            Some(event) = recv(&mut events) => {
                if session.send_speech_event(event).await.is_err() {
                    return SessionEnd::ConnectionLost;
                }
            }
            input = inputs.recv() => match input {
                Some(SessionInput::Key(key)) => {
                    if session.send_key(key).await.is_err() {
                        return SessionEnd::ConnectionLost;
                    }
                }
                Some(SessionInput::End) | None => {
                    session.end().await;
                    return SessionEnd::ByController;
                }
            },
        }
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
