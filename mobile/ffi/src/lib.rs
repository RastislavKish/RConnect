//! Kotlin bindings of the controller core, generated with UniFFI.
//!
//! The Android app creates one [`RConnect`] with its private data directory. It keeps the
//! controller's secret key and the list of paired hosts there.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rconnect_core::speech::{self, SpeechEngine};
use rconnect_core::{
    ConnectError, Controller, Discovery, Host, PairError, SessionEnd, SessionInput,
    SessionObserver, SpeechConfig, drive_session,
};
use rconnect_protocol::messages::{EndReason, KeyMessage, SpeechKind, Utterance};
use rconnect_protocol::ticket::PairingTicket;
use rconnect_protocol::{DeviceId, hex, words};
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
    /// Result of `SessionHandle::ping`: the round trip time in milliseconds, or none when
    /// the computer did not answer in time.
    fn on_ping(&self, milliseconds: Option<f64>);
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
        self.add_host(host)
    }

    /// Searches the local network for computers running RConnect until the returned
    /// handle is stopped. Found computers are reported with their verification words.
    pub async fn start_discovery(
        &self,
        listener: Arc<dyn DiscoveryListener>,
    ) -> Arc<DiscoveryHandle> {
        let controller = self.controller.clone();
        let (found_tx, mut found) = mpsc::unbounded_channel();
        let search = tokio::spawn({
            let controller = controller.clone();
            async move { controller.discover(found_tx).await }
        });
        let paired: Vec<DeviceId> = self
            .hosts
            .lock()
            .unwrap()
            .iter()
            .map(|host| host.id)
            .collect();
        let report = tokio::spawn(async move {
            while let Some(discovery) = found.recv().await {
                match discovery {
                    Discovery::Found { id, name } => listener.on_found(DiscoveredHost {
                        id: hex::encode(&id),
                        name,
                        words: controller.pairing_words(&id).map(String::from).to_vec(),
                        paired: paired.contains(&id),
                    }),
                    Discovery::Lost { id } => listener.on_lost(hex::encode(&id)),
                }
            }
        });
        Arc::new(DiscoveryHandle {
            tasks: vec![search, report],
        })
    }

    /// Pairs with a computer found on the local network. Returns once the user decided on
    /// the computer.
    pub async fn pair_discovered(&self, host_id: String) -> Result<HostInfo, RConnectError> {
        let id = hex::decode::<32>(&host_id)
            .ok_or_else(|| RConnectError::Failed("invalid computer id".into()))?;
        let host = self.controller.pair_discovered(id).await?;
        self.add_host(host)
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
        voice: VoiceSettings,
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
            voice: voice.into(),
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

impl RConnect {
    fn add_host(&self, host: Host) -> Result<HostInfo, RConnectError> {
        let info = HostInfo::from(&host);
        let mut hosts = self.hosts.lock().unwrap();
        hosts.retain(|existing| existing.id != host.id);
        hosts.push(host);
        save_hosts(&self.data_dir, &hosts)?;
        Ok(info)
    }
}

/// A computer found on the local network.
#[derive(Debug, Clone, uniffi::Record)]
pub struct DiscoveredHost {
    /// Hex encoded id, passed to `pair_discovered`.
    pub id: String,
    pub name: String,
    /// Verification words for pairing with it; the computer shows the same ones.
    pub words: Vec<String>,
    /// Whether this phone is already paired with it.
    pub paired: bool,
}

/// Receives the results of a local network search. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait DiscoveryListener: Send + Sync {
    fn on_found(&self, host: DiscoveredHost);
    /// The computer with this id left the network.
    fn on_lost(&self, id: String);
}

/// A running local network search.
#[derive(uniffi::Object)]
pub struct DiscoveryHandle {
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

#[uniffi::export]
impl DiscoveryHandle {
    pub fn stop(&self) {
        self.tasks.iter().for_each(|task| task.abort());
    }
}

impl Drop for DiscoveryHandle {
    fn drop(&mut self) {
        self.stop();
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

    /// Measures the round trip time to the computer; the listener's `on_ping` follows.
    pub fn ping(&self) {
        let _ = self.inputs.send(SessionInput::Ping);
    }

    /// Ends the session; the listener's `on_ended` follows.
    pub fn end(&self) {
        let _ = self.inputs.send(SessionInput::End);
    }
}

/// The phone's speech settings; see the settings screen.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VoiceSettings {
    /// Language voice id, e.g. `en-us`.
    pub voice: String,
    /// Variant id, e.g. `klatt`, or none.
    pub variant: Option<String>,
    /// 0..=100, mapped to 80..=450 words per minute.
    pub rate: u8,
    /// Triples the rate.
    pub rate_boost: bool,
    /// 0..=100, 50 is normal.
    pub pitch: u8,
    /// 0..=100, 100 is normal.
    pub volume: u8,
}

impl From<VoiceSettings> for speech::VoiceSettings {
    fn from(v: VoiceSettings) -> Self {
        speech::VoiceSettings {
            voice: v.voice,
            variant: v.variant,
            rate: v.rate,
            rate_boost: v.rate_boost,
            pitch: v.pitch,
            volume: v.volume,
        }
    }
}

/// The default speech settings.
#[uniffi::export]
pub fn default_voice_settings() -> VoiceSettings {
    let d = speech::VoiceSettings::default();
    VoiceSettings {
        voice: d.voice,
        variant: d.variant,
        rate: d.rate,
        rate_boost: d.rate_boost,
        pitch: d.pitch,
        volume: d.volume,
    }
}

/// Speaking rate in words per minute for the given settings.
#[uniffi::export]
pub fn words_per_minute(settings: VoiceSettings) -> u32 {
    speech::VoiceSettings::from(settings).words_per_minute()
}

/// A language voice or variant offered in the settings.
#[derive(Debug, Clone, uniffi::Record)]
pub struct VoiceOption {
    pub id: String,
    pub name: String,
}

/// Language voices in the speech data directory, sorted by name.
#[uniffi::export]
pub fn list_voices(speech_data_dir: String) -> Vec<VoiceOption> {
    speech::voices::list_voices(Path::new(&speech_data_dir))
        .into_iter()
        .map(|v| VoiceOption {
            id: v.id,
            name: v.name,
        })
        .collect()
}

/// Voice variants in the speech data directory, sorted by name.
#[uniffi::export]
pub fn list_variants(speech_data_dir: String) -> Vec<VoiceOption> {
    speech::voices::list_variants(Path::new(&speech_data_dir))
        .into_iter()
        .map(|v| VoiceOption {
            id: v.id,
            name: v.name,
        })
        .collect()
}

/// Speaks samples with the settings being edited. eSpeak NG can run only once per
/// process, so the preview must be closed before a session starts.
#[derive(uniffi::Object)]
pub struct SpeechPreview {
    engine: Mutex<Option<SpeechEngine>>,
    next_id: std::sync::atomic::AtomicU32,
}

#[uniffi::export]
impl SpeechPreview {
    #[uniffi::constructor]
    pub fn new(speech_data_dir: String, voice: VoiceSettings) -> Result<Arc<Self>, RConnectError> {
        let (engine, _events) =
            SpeechEngine::start(PathBuf::from(speech_data_dir), voice.into(), false)?;
        Ok(Arc::new(SpeechPreview {
            engine: Mutex::new(Some(engine)),
            next_id: std::sync::atomic::AtomicU32::new(1),
        }))
    }

    /// Speaks `text` with `voice`, interrupting any previous sample.
    pub fn speak(&self, voice: VoiceSettings, text: String) {
        if let Some(engine) = self.engine.lock().unwrap().as_ref() {
            engine.set_voice(voice.into());
            let id = self
                .next_id
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            engine.speak(Utterance {
                id,
                kind: SpeechKind::Text,
                text,
                marks: Vec::new(),
            });
        }
    }

    /// Stops the engine and releases eSpeak NG and the audio output.
    pub fn shutdown(&self) {
        self.engine.lock().unwrap().take();
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

    fn ping(&self, round_trip: Option<std::time::Duration>) {
        self.0
            .on_ping(round_trip.map(|duration| duration.as_secs_f64() * 1000.0));
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
