//! The daemon's end of the speech module link (see [`crate::speech::ipc`]).
//!
//! [`SpeechHub`] accepts the module's connection and connects it to the active session:
//! speech from the module goes to the session's speech stream, and playback events from
//! the controller go back to the module.

use std::sync::{Arc, Mutex};

use rconnect_protocol::framing;
use rconnect_protocol::messages::{SpeechCommand, SpeechEvent, SpeechSettings};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;

use crate::paths;
use crate::speech::ipc::{DaemonMessage, IPC_VERSION, ModuleMessage, OutputMode};

pub type SpeechSink = mpsc::UnboundedSender<SpeechCommand>;

#[derive(Default)]
pub struct SpeechHub {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// Connection id and sender of the connected module.
    module: Option<(u64, mpsc::UnboundedSender<DaemonMessage>)>,
    next_module_id: u64,
    /// Latest settings reported by the module.
    settings: Option<SpeechSettings>,
    session: Option<Attached>,
}

struct Attached {
    token: u64,
    sink: SpeechSink,
    mode: OutputMode,
}

impl Inner {
    fn mode(&self) -> OutputMode {
        self.session
            .as_ref()
            .map_or(OutputMode::Local, |session| session.mode)
    }

    fn send_to_module(&self, message: DaemonMessage) {
        if let Some((_, module)) = &self.module {
            let _ = module.send(message);
        }
    }
}

impl SpeechHub {
    pub fn module_connected(&self) -> bool {
        self.inner.lock().unwrap().module.is_some()
    }

    /// Routes speech to a session. `token` identifies the session in [`Self::detach`].
    pub fn attach(&self, token: u64, sink: SpeechSink, mode: OutputMode) {
        let mut inner = self.inner.lock().unwrap();
        // Always sent, even before the module has reported settings: the speech stream
        // only becomes visible to the controller once something is written to it.
        let settings = inner.settings.clone().unwrap_or_default();
        let _ = sink.send(SpeechCommand::Settings(settings));
        inner.session = Some(Attached { token, sink, mode });
        inner.send_to_module(DaemonMessage::SetMode(mode));
    }

    /// Returns speech to the local synthesizer, unless another session has attached since.
    pub fn detach(&self, token: u64) {
        let mut inner = self.inner.lock().unwrap();
        if inner
            .session
            .as_ref()
            .is_some_and(|session| session.token == token)
        {
            inner.session = None;
            inner.send_to_module(DaemonMessage::SetMode(OutputMode::Local));
        }
    }

    /// Playback progress from the controller.
    pub fn event(&self, event: SpeechEvent) {
        self.inner
            .lock()
            .unwrap()
            .send_to_module(DaemonMessage::Event(event));
    }

    fn on_module_message(&self, message: ModuleMessage) {
        let mut inner = self.inner.lock().unwrap();
        let ModuleMessage::Speech(command) = message else {
            return;
        };
        if let SpeechCommand::Settings(settings) = &command {
            inner.settings = Some(settings.clone());
        }
        if let Some(session) = &inner.session {
            let _ = session.sink.send(command);
        }
    }

    pub async fn serve(self: Arc<Self>) -> anyhow::Result<()> {
        let socket = paths::speech_socket();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        loop {
            let (stream, _) = listener.accept().await?;
            tokio::spawn(self.clone().handle_module(stream));
        }
    }

    async fn handle_module(self: Arc<Self>, stream: UnixStream) {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        match framing::read_message::<_, ModuleMessage>(&mut reader).await {
            Ok(Some(ModuleMessage::Hello { version })) if version == IPC_VERSION => {}
            other => {
                tracing::warn!("rejecting speech module connection: {other:?}");
                return;
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let id = {
            let mut inner = self.inner.lock().unwrap();
            let id = inner.next_module_id;
            inner.next_module_id += 1;
            let _ = tx.send(DaemonMessage::SetMode(inner.mode()));
            inner.module = Some((id, tx));
            id
        };
        tracing::info!("speech module connected");

        let writer_task = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                if framing::write_message(&mut writer, &message).await.is_err() {
                    break;
                }
            }
        });

        loop {
            match framing::read_message::<_, ModuleMessage>(&mut reader).await {
                Ok(Some(message)) => self.on_module_message(message),
                Err(e) if e.is_recoverable() => tracing::warn!("ignoring module message: {e}"),
                Ok(None) | Err(_) => break,
            }
        }

        writer_task.abort();
        let mut inner = self.inner.lock().unwrap();
        if inner
            .module
            .as_ref()
            .is_some_and(|(current, _)| *current == id)
        {
            inner.module = None;
        }
        tracing::info!("speech module disconnected");
    }
}
