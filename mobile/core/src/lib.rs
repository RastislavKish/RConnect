//! Controller side of RConnect: pairing with hosts and running sessions.
//!
//! This is the Rust core of the Android app. The app provides storage for the secret key
//! and the list of hosts, captures keys, and plays speech; this crate does the networking.
//! Bindings for Kotlin will be added on top of this API.

mod driver;
pub mod mic;
pub mod speech;

pub use driver::{
    AudioState, MicState, SessionInput, SessionObserver, SpeechConfig, drive_session,
};

use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use iroh::endpoint::{Connection, QuicTransportConfig, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayUrl, SecretKey, TransportAddr};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use rconnect_protocol::messages::{
    AudioDirection, AudioFormat, AudioMode, AudioRequest, Capabilities, ControlMessage, EndReason,
    Handshake, KeyMessage, MicMode, PairRejectReason, PairRequest, PairResponse, RejectReason,
    SpeechCommand, SpeechEvent,
};
use rconnect_protocol::ticket::{PairingTicket, SECRET_LEN};
use rconnect_protocol::{
    ALPN_PAIR, ALPN_SESSION, DeviceId, IDLE_TIMEOUT, KEEP_ALIVE_INTERVAL, MDNS_SERVICE_NAME,
    PROTOCOL_VERSION, framing, words,
};
use serde::{Deserialize, Serialize};
use tokio::io::BufReader;
use tokio::sync::mpsc;

/// Priority of the keys stream, above everything else.
const KEYS_PRIORITY: i32 = 20;

/// A host as remembered by the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Host {
    pub id: DeviceId,
    pub name: String,
    /// Address hints from the pairing ticket; the host is also found through DNS and mDNS.
    pub relay_url: Option<String>,
    pub direct_addrs: Vec<SocketAddr>,
}

impl Host {
    fn endpoint_addr(&self) -> anyhow::Result<EndpointAddr> {
        let id = EndpointId::from_bytes(&self.id)?;
        let mut addrs: Vec<TransportAddr> = self
            .direct_addrs
            .iter()
            .copied()
            .map(TransportAddr::Ip)
            .collect();
        if let Some(relay) = &self.relay_url
            && let Ok(relay) = RelayUrl::from_str(relay)
        {
            addrs.push(TransportAddr::Relay(relay));
        }
        Ok(EndpointAddr::from_parts(id, addrs))
    }

    /// Fingerprint words of the host, for the device details screen.
    pub fn fingerprint(&self) -> [&'static str; words::FINGERPRINT_WORD_COUNT] {
        words::fingerprint_words(&self.id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("the computer declined the pairing")]
    Declined,
    #[error("the pairing code is no longer valid; run `rconnect pair` again")]
    InvalidSecret,
    #[error("pairing is not open on the computer; run `rconnect pair` there first")]
    NotPairing,
    #[error("the computer runs an incompatible version of RConnect")]
    UnsupportedVersion,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    #[error("this phone is not paired with the computer")]
    NotPaired,
    #[error("another phone is controlling the computer")]
    Busy,
    #[error("the computer runs an incompatible version of RConnect")]
    UnsupportedVersion,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// A computer found on the local network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    Found { id: DeviceId, name: String },
    Lost { id: DeviceId },
}

/// The controller side of RConnect. Cheap to clone; clones share the endpoint.
#[derive(Clone)]
pub struct Controller {
    endpoint: Endpoint,
    mdns: MdnsAddressLookup,
    device_name: String,
}

impl Controller {
    /// Starts the controller's endpoint with its persistent secret key.
    pub async fn new(secret_key: [u8; 32], device_name: String) -> anyhow::Result<Controller> {
        let transport = QuicTransportConfig::builder()
            .max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?))
            .keep_alive_interval(KEEP_ALIVE_INTERVAL)
            // On a congested link, old microphone audio is dropped rather than queued
            // ahead of keys.
            .datagram_send_buffer_size(2048)
            .build();
        let secret_key = SecretKey::from_bytes(&secret_key);
        // Built here rather than by the endpoint, so discovery can subscribe to it.
        let mdns = MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE_NAME)
            .advertise(false)
            .build(secret_key.public())?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .transport_config(transport)
            .address_lookup(mdns.clone())
            .bind()
            .await?;
        Ok(Controller {
            endpoint,
            mdns,
            device_name,
        })
    }

    pub fn generate_secret_key() -> [u8; 32] {
        SecretKey::generate().to_bytes()
    }

    pub fn id(&self) -> DeviceId {
        *self.endpoint.id().as_bytes()
    }

    /// Words to show while pairing; the host shows the same ones.
    pub fn pairing_words(&self, host: &DeviceId) -> [&'static str; words::PAIRING_WORD_COUNT] {
        words::pairing_words(host, &self.id())
    }

    /// Pairs with the host from a scanned ticket. Waits until the user decides on the host.
    pub async fn pair(&self, ticket: &PairingTicket) -> Result<Host, PairError> {
        let host = Host {
            id: ticket.host_id,
            name: String::new(),
            relay_url: ticket.relay_url.clone(),
            direct_addrs: ticket.direct_addrs.clone(),
        };
        self.pair_with(host, Some(ticket.secret.0)).await
    }

    /// Pairs with a host found on the local network. Without a ticket's secret, the
    /// verification words are what protects this pairing, so they must be shown.
    pub async fn pair_discovered(&self, host_id: DeviceId) -> Result<Host, PairError> {
        let host = Host {
            id: host_id,
            name: String::new(),
            relay_url: None,
            direct_addrs: Vec::new(),
        };
        self.pair_with(host, None).await
    }

    async fn pair_with(
        &self,
        mut host: Host,
        secret: Option<[u8; SECRET_LEN]>,
    ) -> Result<Host, PairError> {
        let request = PairRequest::Pair {
            protocol_version: PROTOCOL_VERSION,
            secret,
            device_name: self.device_name.clone(),
        };
        let connection = self.connect(&host, ALPN_PAIR).await?;
        let (mut send, recv) = connection.open_bi().await.context("cannot open stream")?;
        framing::write_message(&mut send, &request)
            .await
            .context("cannot send request")?;
        send.finish().context("cannot send request")?;

        let response = framing::read_message::<_, PairResponse>(&mut BufReader::new(recv))
            .await
            .context("no answer from the computer")?;
        connection.close(0u32.into(), b"done");
        match response {
            Some(PairResponse::Accepted { device_name }) => {
                host.name = device_name;
                Ok(host)
            }
            Some(PairResponse::Rejected(reason)) => Err(match reason {
                PairRejectReason::Declined => PairError::Declined,
                PairRejectReason::InvalidSecret => PairError::InvalidSecret,
                PairRejectReason::NotPairing => PairError::NotPairing,
                PairRejectReason::UnsupportedVersion(_) => PairError::UnsupportedVersion,
            }),
            other => Err(anyhow::anyhow!("unexpected answer {other:?}").into()),
        }
    }

    /// Reports computers on the local network until `found` is closed. Each one is asked
    /// for its name.
    pub async fn discover(&self, found: mpsc::UnboundedSender<Discovery>) {
        use tokio_stream::StreamExt;

        let mut events = self.mdns.subscribe().await;
        let mut known = std::collections::HashSet::new();
        loop {
            let event = tokio::select! {
                event = events.next() => match event {
                    Some(event) => event,
                    None => return,
                },
                _ = found.closed() => return,
            };
            match event {
                DiscoveryEvent::Discovered { endpoint_info, .. } => {
                    let id = *endpoint_info.endpoint_id.as_bytes();
                    if !known.insert(id) {
                        continue;
                    }
                    let controller = self.clone();
                    let found = found.clone();
                    tokio::spawn(async move {
                        match controller.query_name(&id).await {
                            Ok(name) => {
                                let _ = found.send(Discovery::Found { id, name });
                            }
                            Err(e) => {
                                tracing::debug!("computer on the network did not answer: {e:#}")
                            }
                        }
                    });
                }
                DiscoveryEvent::Expired { endpoint_id } => {
                    let id = *endpoint_id.as_bytes();
                    if known.remove(&id) {
                        let _ = found.send(Discovery::Lost { id });
                    }
                }
                _ => {}
            }
        }
    }

    /// Asks a host for its name.
    pub async fn query_name(&self, host_id: &DeviceId) -> anyhow::Result<String> {
        let host = Host {
            id: *host_id,
            name: String::new(),
            relay_url: None,
            direct_addrs: Vec::new(),
        };
        let connection = self.connect(&host, ALPN_PAIR).await?;
        let (mut send, recv) = connection.open_bi().await?;
        framing::write_message(&mut send, &PairRequest::Query).await?;
        send.finish()?;
        let response = tokio::time::timeout(
            Duration::from_secs(5),
            framing::read_message::<_, PairResponse>(&mut BufReader::new(recv)),
        )
        .await??;
        connection.close(0u32.into(), b"done");
        match response {
            Some(PairResponse::Info { device_name, .. }) => Ok(device_name),
            other => anyhow::bail!("unexpected answer {other:?}"),
        }
    }

    /// Starts a session with a paired host.
    pub async fn start_session(&self, host: &Host) -> Result<Session, ConnectError> {
        let connection = self.connect(host, ALPN_SESSION).await?;
        let (mut control_send, control_recv) =
            connection.open_bi().await.context("cannot open stream")?;
        let mut control_recv = BufReader::new(control_recv);
        let hello = ControlMessage::Hello(Handshake {
            protocol_version: PROTOCOL_VERSION,
            app_version: env!("CARGO_PKG_VERSION").into(),
            device_name: self.device_name.clone(),
            capabilities: Capabilities::AUDIO_TO_CONTROLLER | Capabilities::AUDIO_TO_HOST,
        });
        framing::write_message(&mut control_send, &hello)
            .await
            .context("cannot send hello")?;

        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            framing::read_message::<_, ControlMessage>(&mut control_recv),
        )
        .await
        .context("no answer from the computer")?
        .context("no answer from the computer")?;
        let welcome = match answer {
            Some(ControlMessage::Welcome(welcome)) => welcome,
            Some(ControlMessage::Reject(reason)) => {
                return Err(match reason {
                    RejectReason::NotPaired => ConnectError::NotPaired,
                    RejectReason::Busy => ConnectError::Busy,
                    RejectReason::UnsupportedVersion(_) => ConnectError::UnsupportedVersion,
                });
            }
            other => return Err(anyhow::anyhow!("unexpected answer {other:?}").into()),
        };

        let keys = connection
            .open_uni()
            .await
            .context("cannot open keys stream")?;
        keys.set_priority(KEYS_PRIORITY)
            .context("cannot open keys stream")?;
        let (speech_send, speech_recv) =
            tokio::time::timeout(Duration::from_secs(10), connection.accept_bi())
                .await
                .context("no speech stream from the computer")?
                .context("no speech stream from the computer")?;

        let (output_tx, output_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_speech(BufReader::new(speech_recv), output_tx.clone()));
        tokio::spawn(read_control(control_recv, output_tx));

        Ok(Session {
            host_name: welcome.device_name,
            host_capabilities: welcome.capabilities,
            connection,
            control: control_send,
            keys,
            speech_events: speech_send,
            outputs: Some(output_rx),
        })
    }

    async fn connect(&self, host: &Host, alpn: &[u8]) -> anyhow::Result<Connection> {
        let addr = host.endpoint_addr()?;
        tokio::time::timeout(Duration::from_secs(15), self.endpoint.connect(addr, alpn))
            .await
            .context("the computer did not answer in time")?
            .context("cannot connect to the computer")
    }

    pub async fn shutdown(self) {
        self.endpoint.close().await;
    }
}

/// Why the session ended, from the controller's point of view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionEnd {
    /// The host ended it.
    ByHost(EndReason),
    /// It was ended on this device.
    ByController,
    /// The connection was lost.
    ConnectionLost,
}

/// What the host sends during a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOutput {
    Speech(SpeechCommand),
    /// The host's answer to [`Session::send_ping`].
    Pong(u32),
    /// The host streams audio as datagrams (see [`Session::connection`]).
    AudioStarted(AudioFormat),
    /// The host stopped streaming audio, or cannot stream it.
    AudioStopped,
    /// The host's answer to [`Session::request_mic`].
    MicState(MicMode),
    /// The host cannot create the virtual microphone.
    MicUnavailable,
    /// The session is over; nothing follows.
    Ended(SessionEnd),
}

/// The network path a session's data takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionPath {
    /// Straight to the host's IP address.
    Direct,
    /// Through an iroh relay server.
    Relayed,
    /// No path is selected, e.g. while the connection is closing.
    Unknown,
}

pub struct Session {
    pub host_name: String,
    pub host_capabilities: Capabilities,
    connection: Connection,
    control: SendStream,
    keys: SendStream,
    speech_events: SendStream,
    outputs: Option<mpsc::UnboundedReceiver<SessionOutput>>,
}

impl Session {
    pub async fn send_key(&mut self, message: KeyMessage) -> anyhow::Result<()> {
        framing::write_message(&mut self.keys, &message).await?;
        Ok(())
    }

    /// The path currently used for sending data to the host.
    pub fn path(&self) -> ConnectionPath {
        let paths = self.connection.paths();
        match paths.iter().find(|path| path.is_selected()) {
            Some(path) if path.is_relay() => ConnectionPath::Relayed,
            Some(_) => ConnectionPath::Direct,
            None => ConnectionPath::Unknown,
        }
    }

    /// Asks the host to answer with [`SessionOutput::Pong`] carrying `number`.
    pub async fn send_ping(&mut self, number: u32) -> anyhow::Result<()> {
        framing::write_message(&mut self.control, &ControlMessage::Ping(number)).await?;
        Ok(())
    }

    /// Asks the host to stream its audio in `mode`, or to stop with `None`.
    pub async fn request_audio(&mut self, mode: Option<AudioMode>) -> anyhow::Result<()> {
        let message = match mode {
            Some(mode) => ControlMessage::AudioRequest(AudioRequest {
                direction: AudioDirection::ToController,
                mode,
            }),
            None => ControlMessage::AudioStop,
        };
        framing::write_message(&mut self.control, &message).await?;
        Ok(())
    }

    /// Sets the microphone's mode on the host. In Unmuted and VAD, the microphone's
    /// datagrams are sent by a [`crate::mic::MicStream`].
    pub async fn request_mic(&mut self, mode: MicMode) -> anyhow::Result<()> {
        framing::write_message(&mut self.control, &ControlMessage::MicRequest(mode)).await?;
        Ok(())
    }

    /// The connection, e.g. to read audio datagrams from another task.
    pub fn connection(&self) -> Connection {
        self.connection.clone()
    }

    pub async fn send_speech_event(&mut self, event: SpeechEvent) -> anyhow::Result<()> {
        framing::write_message(&mut self.speech_events, &event).await?;
        Ok(())
    }

    /// Takes the receiver of everything the host sends, so it can be awaited while keys
    /// are being sent. Returns `None` after the first call. When the channel closes
    /// without [`SessionOutput::Ended`], the connection was lost.
    pub fn take_outputs(&mut self) -> Option<mpsc::UnboundedReceiver<SessionOutput>> {
        self.outputs.take()
    }

    /// Ends the session: releases all keys on the host and says goodbye.
    pub async fn end(mut self) {
        let _ = self.send_key(KeyMessage::ReleaseAll).await;
        let goodbye = ControlMessage::EndSession(EndReason::UserRequest);
        let _ = framing::write_message(&mut self.control, &goodbye).await;
        let _ = self.control.finish();
        let _ = tokio::time::timeout(Duration::from_secs(1), self.connection.closed()).await;
        self.connection.close(0u32.into(), b"bye");
    }
}

async fn read_speech(mut stream: BufReader<RecvStream>, tx: mpsc::UnboundedSender<SessionOutput>) {
    loop {
        match framing::read_message::<_, SpeechCommand>(&mut stream).await {
            Ok(Some(command)) => {
                if tx.send(SessionOutput::Speech(command)).is_err() {
                    return;
                }
            }
            Err(e) if e.is_recoverable() => tracing::warn!("ignoring speech command: {e}"),
            Ok(None) | Err(_) => return,
        }
    }
}

async fn read_control(mut stream: BufReader<RecvStream>, tx: mpsc::UnboundedSender<SessionOutput>) {
    let end = loop {
        match framing::read_message::<_, ControlMessage>(&mut stream).await {
            Ok(Some(ControlMessage::EndSession(reason))) => break SessionEnd::ByHost(reason),
            Ok(Some(message)) => {
                let output = match message {
                    ControlMessage::Pong(number) => SessionOutput::Pong(number),
                    ControlMessage::AudioStarted(format) => SessionOutput::AudioStarted(format),
                    ControlMessage::AudioStop => SessionOutput::AudioStopped,
                    ControlMessage::MicState(mode) => SessionOutput::MicState(mode),
                    ControlMessage::MicUnavailable => SessionOutput::MicUnavailable,
                    _ => continue,
                };
                if tx.send(output).is_err() {
                    return;
                }
            }
            Err(e) if e.is_recoverable() => {}
            Ok(None) | Err(_) => break SessionEnd::ConnectionLost,
        }
    };
    let _ = tx.send(SessionOutput::Ended(end));
}
