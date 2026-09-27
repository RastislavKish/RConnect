//! Kotlin bindings of the controller core, generated with UniFFI.
//!
//! The Android app creates one [`RConnect`] with its private data directory. It keeps the
//! controller's secret key and the list of paired hosts there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rconnect_core::{
    ConnectError, Controller, Host, PairError, SessionEnd, SessionInput, SessionObserver,
    SpeechConfig, drive_session,
};
use rconnect_protocol::messages::{EndReason, KeyMessage};
use rconnect_protocol::ticket::PairingTicket;
use rconnect_protocol::{hex, words};
use tokio::sync::mpsc;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum RConnectError {
    #[error("{0}")]
    InvalidTicket(String),
    #[error("{0}")]
    Declined(String),
    #[error("{0}")]
    NotPaired(String),
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    Failed(String),
}

impl From<anyhow::Error> for RConnectError {
    fn from(e: anyhow::Error) -> Self {
        RConnectError::Failed(format!("{e:#}"))
    }
}

impl From<PairError> for RConnectError {
    fn from(e: PairError) -> Self {
        match e {
            PairError::Declined => RConnectError::Declined(e.to_string()),
            other => RConnectError::Failed(format!("{other:#}")),
        }
    }
}

impl From<ConnectError> for RConnectError {
    fn from(e: ConnectError) -> Self {
        match e {
            ConnectError::NotPaired => RConnectError::NotPaired(e.to_string()),
            ConnectError::Busy => RConnectError::Busy(e.to_string()),
            other => RConnectError::Failed(format!("{other:#}")),
        }
    }
}

/// A paired computer, as shown in the list.
#[derive(Debug, Clone, uniffi::Record)]
pub struct HostInfo {
    /// Hex encoded id, used to refer to the host.
    pub id: String,
    pub name: String,
    /// Fingerprint words, to check the pairing again.
    pub fingerprint: Vec<String>,
}

impl From<&Host> for HostInfo {
    fn from(host: &Host) -> Self {
        HostInfo {
            id: hex::encode(&host.id),
            name: host.name.clone(),
            fingerprint: host.fingerprint().map(String::from).to_vec(),
        }
    }
}

/// What the app shows while pairing from a scanned or pasted ticket.
#[derive(Debug, Clone, uniffi::Record)]
pub struct TicketInfo {
    /// Verification words; the computer shows the same ones.
    pub words: Vec<String>,
}

/// Receives what happens in a session. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait SessionListener: Send + Sync {
    /// Text the computer's screen reader is speaking.
    fn on_speech(&self, text: String);
    /// The session ended; `reason` is a sentence to announce.
    fn on_ended(&self, reason: String);
}

#[derive(uniffi::Object)]
pub struct RConnect {
    controller: Controller,
    data_dir: PathBuf,
    hosts: Mutex<Vec<Host>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl RConnect {
    /// Starts the controller. `data_dir` is the app's private directory.
    #[uniffi::constructor]
    pub async fn create(
        data_dir: String,
        device_name: String,
    ) -> Result<Arc<RConnect>, RConnectError> {
        let data_dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&data_dir).map_err(anyhow::Error::from)?;
        let secret_key = load_secret_key(&data_dir)?;
        let hosts = load_hosts(&data_dir)?;
        let controller = Controller::new(secret_key, device_name).await?;
        Ok(Arc::new(RConnect {
            controller,
            data_dir,
            hosts: Mutex::new(hosts),
        }))
    }

    /// This phone's fingerprint words.
    pub fn fingerprint(&self) -> Vec<String> {
        words::fingerprint_words(&self.controller.id())
            .map(String::from)
            .to_vec()
    }

    pub fn hosts(&self) -> Vec<HostInfo> {
        self.hosts
            .lock()
            .unwrap()
            .iter()
            .map(HostInfo::from)
            .collect()
    }

    /// Checks a ticket and returns the words to show before pairing.
    pub fn read_ticket(&self, uri: String) -> Result<TicketInfo, RConnectError> {
        let ticket = parse_ticket(&uri)?;
        let words = self.controller.pairing_words(&ticket.host_id);
        Ok(TicketInfo {
            words: words.map(String::from).to_vec(),
        })
    }

    /// Pairs with the computer from a ticket. Returns once the user decided on the
    /// computer.
    pub async fn pair(&self, uri: String) -> Result<HostInfo, RConnectError> {
        let ticket = parse_ticket(&uri)?;
        let host = self.controller.pair(&ticket).await?;
        let info = HostInfo::from(&host);
        let mut hosts = self.hosts.lock().unwrap();
        hosts.retain(|existing| existing.id != host.id);
        hosts.push(host);
        save_hosts(&self.data_dir, &hosts)?;
        Ok(info)
    }

    /// Forgets a paired computer.
    pub fn remove_host(&self, id: String) -> Result<(), RConnectError> {
        let mut hosts = self.hosts.lock().unwrap();
        hosts.retain(|host| hex::encode(&host.id) != id);
        save_hosts(&self.data_dir, &hosts)?;
        Ok(())
    }

    /// Connects to a paired computer and runs the session in the background.
    /// `speech_data_dir` contains `espeak-ng-data`.
    pub async fn start_session(
        &self,
        host_id: String,
        speech_data_dir: String,
        keep_alive: bool,
        listener: Arc<dyn SessionListener>,
    ) -> Result<Arc<SessionHandle>, RConnectError> {
        let host = self
            .hosts
            .lock()
            .unwrap()
            .iter()
            .find(|host| hex::encode(&host.id) == host_id)
            .cloned()
            .ok_or_else(|| RConnectError::Failed("unknown computer".into()))?;
        let session = self.controller.start_session(&host).await?;

        let (inputs_tx, inputs) = mpsc::unbounded_channel();
        let speech = SpeechConfig {
            data_dir: PathBuf::from(speech_data_dir),
            keep_alive,
        };
        tokio::spawn(async move {
            let observer = Observer(listener.clone());
            let end = drive_session(session, inputs, speech, &observer).await;
            listener.on_ended(describe_end(&end).into());
        });
        Ok(Arc::new(SessionHandle { inputs: inputs_tx }))
    }
}

/// Controls a running session.
#[derive(uniffi::Object)]
pub struct SessionHandle {
    inputs: mpsc::UnboundedSender<SessionInput>,
}

#[uniffi::export]
impl SessionHandle {
    /// Sends a key press or release. `code` is a Linux evdev key code.
    pub fn send_key(&self, code: u16, down: bool) {
        let key = if down {
            KeyMessage::Down(code)
        } else {
            KeyMessage::Up(code)
        };
        let _ = self.inputs.send(SessionInput::Key(key));
    }

    /// Releases every key held on the computer.
    pub fn release_all(&self) {
        let _ = self.inputs.send(SessionInput::Key(KeyMessage::ReleaseAll));
    }

    /// Ends the session; the listener's `on_ended` follows.
    pub fn end(&self) {
        let _ = self.inputs.send(SessionInput::End);
    }
}

/// Sends Rust logs to logcat under the tag `RConnect`.
#[uniffi::export]
pub fn init_logging() {
    #[cfg(target_os = "android")]
    logcat::init();
}

struct Observer(Arc<dyn SessionListener>);

impl SessionObserver for Observer {
    fn speech(&self, text: &str) {
        self.0.on_speech(text.to_string());
    }
}

fn describe_end(end: &SessionEnd) -> &'static str {
    match end {
        SessionEnd::ByController => "Session ended.",
        SessionEnd::ConnectionLost => "Connection to the computer lost.",
        SessionEnd::ByHost(EndReason::UserRequest) => "The computer ended the session.",
        SessionEnd::ByHost(EndReason::Shutdown) => "RConnect on the computer shut down.",
        SessionEnd::ByHost(EndReason::Replaced) => "The session continues on another connection.",
        SessionEnd::ByHost(EndReason::Revoked) => "The computer removed this phone.",
        SessionEnd::ByHost(EndReason::Error) => "The session ended because of an error.",
    }
}

fn parse_ticket(uri: &str) -> Result<PairingTicket, RConnectError> {
    PairingTicket::from_uri(uri).map_err(|e| RConnectError::InvalidTicket(e.to_string()))
}

fn load_secret_key(dir: &Path) -> anyhow::Result<[u8; 32]> {
    let path = dir.join("secret.key");
    match std::fs::read(&path) {
        Ok(bytes) => bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("corrupted {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let key = Controller::generate_secret_key();
            std::fs::write(&path, key)?;
            Ok(key)
        }
        Err(e) => Err(e.into()),
    }
}

fn load_hosts(dir: &Path) -> anyhow::Result<Vec<Host>> {
    match std::fs::read(dir.join("hosts.json")) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_hosts(dir: &Path, hosts: &[Host]) -> anyhow::Result<()> {
    let temporary = dir.join("hosts.json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(hosts)?)?;
    std::fs::rename(temporary, dir.join("hosts.json"))?;
    Ok(())
}

#[cfg(target_os = "android")]
mod logcat {
    use std::ffi::CString;
    use std::io::Write;

    struct Logcat(Vec<u8>);

    impl Write for Logcat {
        fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
            self.0.extend_from_slice(data);
            Ok(data.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Drop for Logcat {
        fn drop(&mut self) {
            let text = String::from_utf8_lossy(&self.0);
            if let Ok(text) = CString::new(text.trim_end()) {
                // SAFETY: both strings are valid and NUL terminated.
                unsafe {
                    ndk_sys::__android_log_write(
                        ndk_sys::android_LogPriority::ANDROID_LOG_INFO.0 as i32,
                        c"RConnect".as_ptr(),
                        text.as_ptr(),
                    );
                }
            }
        }
    }

    pub fn init() {
        let _ = tracing_subscriber::fmt()
            .with_writer(|| Logcat(Vec::new()))
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .try_init();
    }
}
