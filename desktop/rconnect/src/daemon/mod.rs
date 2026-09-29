//! `rconnect daemon`: accepts pairing and session connections from phones.

mod audio;
mod inhibit;
mod keyboard;
mod pairing;
mod session;
mod speech;

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};

use iroh::endpoint::{Incoming, QuicTransportConfig, presets};
use iroh::{Endpoint, EndpointId};
use iroh_mdns_address_lookup::MdnsAddressLookup;
use rconnect_protocol::{
    ALPN_PAIR, ALPN_SESSION, IDLE_TIMEOUT, KEEP_ALIVE_INTERVAL, MDNS_SERVICE_NAME, framing, hex,
    words,
};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};

use crate::control::{ControlRequest, ControlResponse, Status};
use crate::paths;
use crate::store::{self, Config, Devices};
use keyboard::Keyboard;
use pairing::PairingWindow;
use session::SessionHandle;
use speech::SpeechHub;

pub struct Daemon {
    endpoint: Endpoint,
    config: Config,
    devices: Mutex<Devices>,
    keyboard: Mutex<Keyboard>,
    speech: Arc<SpeechHub>,
    pairing: Mutex<Option<PairingWindow>>,
    session: Mutex<Option<SessionHandle>>,
    next_session_token: AtomicU64,
}

pub fn run() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run_async())
}

async fn run_async() -> anyhow::Result<()> {
    paths::create_runtime_dir()?;
    let config = Config::load()?;
    audio::restore_after_crash();
    let secret_key = store::load_or_create_secret_key()?;

    let transport = QuicTransportConfig::builder()
        .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
        .keep_alive_interval(KEEP_ALIVE_INTERVAL)
        .datagram_send_buffer_size(audio::DATAGRAM_SEND_BUFFER)
        .build();
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(vec![ALPN_SESSION.to_vec(), ALPN_PAIR.to_vec()])
        .transport_config(transport)
        .address_lookup(MdnsAddressLookup::builder().service_name(MDNS_SERVICE_NAME))
        .bind()
        .await?;

    let daemon = Arc::new(Daemon {
        endpoint,
        config,
        devices: Mutex::new(Devices::load()?),
        keyboard: Mutex::new(Keyboard::new()),
        speech: Arc::default(),
        pairing: Mutex::new(None),
        session: Mutex::new(None),
        next_session_token: AtomicU64::new(1),
    });
    tracing::info!(
        "RConnect host {:?} ready, fingerprint: {}",
        daemon.config.name,
        daemon.fingerprint().join(" ")
    );

    tokio::spawn(log_errors("speech socket", daemon.speech.clone().serve()));
    tokio::spawn(log_errors("control socket", daemon.clone().serve_control()));

    let accept = async {
        while let Some(incoming) = daemon.endpoint.accept().await {
            tokio::spawn(daemon.clone().handle_incoming(incoming));
        }
    };
    tokio::select! {
        _ = accept => {}
        _ = shutdown_signal() => tracing::info!("shutting down"),
    }

    daemon.shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let mut terminate = signal(SignalKind::terminate()).expect("signal handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

async fn log_errors(what: &str, task: impl Future<Output = anyhow::Result<()>>) {
    if let Err(e) = task.await {
        tracing::error!("{what}: {e:#}");
    }
}

impl Daemon {
    fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    fn fingerprint(&self) -> Vec<String> {
        words::fingerprint_words(self.id().as_bytes())
            .map(String::from)
            .to_vec()
    }

    async fn handle_incoming(self: Arc<Self>, incoming: Incoming) {
        let connection = match incoming.await {
            Ok(connection) => connection,
            Err(e) => {
                tracing::debug!("incoming connection failed: {e}");
                return;
            }
        };
        let result = match connection.alpn() {
            ALPN_SESSION => self.clone().run_session(connection).await,
            ALPN_PAIR => self.handle_pairing(connection).await,
            _ => Ok(()),
        };
        if let Err(e) = result {
            tracing::warn!("{e:#}");
        }
    }

    async fn shutdown(&self) {
        self.end_session_if(|_| true, rconnect_protocol::messages::EndReason::Shutdown);
        // Give the session a moment to tell the phone and release keys.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        self.endpoint.close().await;
        let _ = std::fs::remove_file(paths::control_socket());
        let _ = std::fs::remove_file(paths::speech_socket());
    }

    // --- control socket ---

    async fn serve_control(self: Arc<Self>) -> anyhow::Result<()> {
        let socket = paths::control_socket();
        let _ = std::fs::remove_file(&socket);
        let listener = UnixListener::bind(&socket)?;
        loop {
            let (stream, _) = listener.accept().await?;
            let daemon = self.clone();
            tokio::spawn(async move {
                if let Err(e) = daemon.handle_control(stream).await {
                    tracing::debug!("control connection: {e:#}");
                }
            });
        }
    }

    async fn handle_control(self: Arc<Self>, stream: UnixStream) -> anyhow::Result<()> {
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let Some(request) = framing::read_message::<_, ControlRequest>(&mut reader).await? else {
            return Ok(());
        };

        let response = match request {
            ControlRequest::Status => ControlResponse::Status(self.status()),
            ControlRequest::ListDevices => {
                ControlResponse::Devices(self.devices.lock().unwrap().list().to_vec())
            }
            ControlRequest::Revoke(query) => {
                let removed = self.devices.lock().unwrap().remove(&query);
                match removed {
                    Ok(removed) => {
                        let ids: Vec<&str> =
                            removed.iter().map(|device| device.id.as_str()).collect();
                        self.end_session_if(
                            |controller| ids.contains(&hex::encode(controller.as_bytes()).as_str()),
                            rconnect_protocol::messages::EndReason::Revoked,
                        );
                        ControlResponse::Revoked(removed)
                    }
                    Err(e) => ControlResponse::Error(format!("{e:#}")),
                }
            }
            ControlRequest::Pair => return self.run_pairing_window(reader, writer).await,
            ControlRequest::PairingDecision { .. } => {
                ControlResponse::Error("no pairing request is waiting".into())
            }
        };
        framing::write_message(&mut writer, &response).await?;
        Ok(())
    }

    fn status(&self) -> Status {
        Status {
            name: self.config.name.clone(),
            id: hex::encode(self.id().as_bytes()),
            fingerprint: self.fingerprint(),
            speech_module_connected: self.speech.module_connected(),
            keyboard_available: self.keyboard.lock().unwrap().is_available(),
            session: self
                .session
                .lock()
                .unwrap()
                .as_ref()
                .map(|session| session.name.clone()),
        }
    }
}
