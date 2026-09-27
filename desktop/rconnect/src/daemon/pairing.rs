//! Pairing: the window opened by `rconnect pair` and the phone's side on [`ALPN_PAIR`].
//!
//! [`ALPN_PAIR`]: rconnect_protocol::ALPN_PAIR

use std::sync::Arc;
use std::time::Duration;

use iroh::EndpointId;
use iroh::endpoint::Connection;
use rconnect_protocol::messages::{PairRejectReason, PairRequest, PairResponse};
use rconnect_protocol::ticket::{PairingSecret, PairingTicket, SECRET_LEN};
use rconnect_protocol::{PROTOCOL_VERSION, framing, words};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

use super::Daemon;
use crate::control::{ControlRequest, ControlResponse, PairingResult};
use crate::store::PairedDevice;

/// How long `rconnect pair` keeps pairing open.
const WINDOW_TIMEOUT: Duration = Duration::from_secs(5 * 60);

/// How long the phone waits for the user's decision.
const DECISION_TIMEOUT: Duration = Duration::from_secs(3 * 60);

pub struct PairingWindow {
    secret: PairingSecret,
    prompts: mpsc::Sender<Prompt>,
}

/// A pairing request waiting for the user's decision in `rconnect pair`.
struct Prompt {
    controller: EndpointId,
    device_name: String,
    reply: oneshot::Sender<bool>,
}

impl Daemon {
    /// Serves one `rconnect pair` invocation for as long as the window is open.
    pub(super) async fn run_pairing_window(
        self: Arc<Self>,
        mut reader: BufReader<OwnedReadHalf>,
        mut writer: OwnedWriteHalf,
    ) -> anyhow::Result<()> {
        let secret = PairingSecret::generate();
        let (prompt_tx, mut prompts) = mpsc::channel(4);
        let opened = {
            let mut pairing = self.pairing.lock().unwrap();
            let free = pairing.is_none();
            if free {
                *pairing = Some(PairingWindow {
                    secret,
                    prompts: prompt_tx,
                });
            }
            free
        };
        if !opened {
            let error = ControlResponse::Error("pairing is already open elsewhere".into());
            framing::write_message(&mut writer, &error).await?;
            return Ok(());
        }
        // Closes the window however this function ends.
        let _close = scopeguard(|| *self.pairing.lock().unwrap() = None);

        let uri = self.ticket(secret).await.to_uri();
        tracing::info!("pairing window open");
        let opened = ControlResponse::PairingOpened {
            uri,
            timeout_secs: WINDOW_TIMEOUT.as_secs(),
        };
        framing::write_message(&mut writer, &opened).await?;

        // Requests from the CLI, read in a separate task because reads are not cancel safe.
        let (decision_tx, mut decisions) = mpsc::channel(1);
        let reader_task = tokio::spawn(async move {
            while let Ok(Some(request)) =
                framing::read_message::<_, ControlRequest>(&mut reader).await
            {
                if decision_tx.send(request).await.is_err() {
                    break;
                }
            }
        });
        let _abort_reader = scopeguard(move || reader_task.abort());

        let deadline = tokio::time::Instant::now() + WINDOW_TIMEOUT;
        loop {
            let prompt = tokio::select! {
                prompt = prompts.recv() => prompt.expect("the window holds a sender"),
                request = decisions.recv() => match request {
                    // The CLI went away (Ctrl+C).
                    None => return Ok(()),
                    Some(_) => continue,
                },
                _ = tokio::time::sleep_until(deadline) => {
                    let finished = ControlResponse::PairingFinished(PairingResult::TimedOut);
                    framing::write_message(&mut writer, &finished).await?;
                    return Ok(());
                }
            };

            let words = words::pairing_words(self.id().as_bytes(), prompt.controller.as_bytes());
            let request = ControlResponse::PairingRequest {
                device_name: prompt.device_name.clone(),
                words: words.map(String::from).to_vec(),
            };
            framing::write_message(&mut writer, &request).await?;

            let accept = tokio::select! {
                request = decisions.recv() => match request {
                    Some(ControlRequest::PairingDecision { accept }) => accept,
                    Some(_) => false,
                    None => return Ok(()),
                },
                _ = tokio::time::sleep_until(deadline) => false,
            };
            if !accept {
                tracing::info!("pairing with {:?} declined", prompt.device_name);
                let _ = prompt.reply.send(false);
                continue;
            }

            let device = PairedDevice::new(&prompt.controller, prompt.device_name.clone());
            self.devices.lock().unwrap().add(device)?;
            tracing::info!("paired with {:?}", prompt.device_name);
            let _ = prompt.reply.send(true);
            let finished = ControlResponse::PairingFinished(PairingResult::Paired {
                device_name: prompt.device_name,
            });
            framing::write_message(&mut writer, &finished).await?;
            return Ok(());
        }
    }

    /// A minimal ticket, so the QR code stays small. The phone finds this host by its id
    /// through mDNS on the local network, or iroh's DNS address lookup elsewhere.
    async fn ticket(&self, secret: PairingSecret) -> PairingTicket {
        // Give the endpoint a moment to come online, so its DNS record is published by
        // the time the phone looks it up.
        let _ = tokio::time::timeout(Duration::from_secs(3), self.endpoint.online()).await;
        PairingTicket {
            host_id: *self.id().as_bytes(),
            secret,
            relay_url: None,
            direct_addrs: Vec::new(),
        }
    }

    /// Handles a phone's connection on the pairing ALPN.
    pub(super) async fn handle_pairing(&self, connection: Connection) -> anyhow::Result<()> {
        let controller = connection.remote_id();
        let (mut send, recv) = connection.accept_bi().await?;
        let mut recv = BufReader::new(recv);
        let request = tokio::time::timeout(
            Duration::from_secs(10),
            framing::read_message::<_, PairRequest>(&mut recv),
        )
        .await??;

        let response = match request {
            None => return Ok(()),
            Some(PairRequest::Query) => PairResponse::Info {
                protocol_version: PROTOCOL_VERSION,
                device_name: self.config.name.clone(),
            },
            Some(PairRequest::Pair {
                protocol_version,
                secret,
                device_name,
            }) => {
                self.pair(controller, protocol_version, secret, device_name)
                    .await
            }
        };
        framing::write_message(&mut send, &response).await?;
        send.finish()?;
        // Let the phone read the response before the connection goes away.
        let _ = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
        Ok(())
    }

    async fn pair(
        &self,
        controller: EndpointId,
        protocol_version: u16,
        secret: Option<[u8; SECRET_LEN]>,
        device_name: String,
    ) -> PairResponse {
        if protocol_version != PROTOCOL_VERSION {
            return PairResponse::Rejected(PairRejectReason::UnsupportedVersion(PROTOCOL_VERSION));
        }
        let device_name = clean_name(&device_name);

        let prompts = {
            let pairing = self.pairing.lock().unwrap();
            let Some(window) = pairing.as_ref() else {
                return PairResponse::Rejected(PairRejectReason::NotPairing);
            };
            // Without a secret the phone found this host on the local network; the
            // verification words are then the only protection, which the user checks.
            if let Some(secret) = secret
                && !window.secret.matches(&secret)
            {
                tracing::warn!("pairing request from {device_name:?} with a wrong secret");
                return PairResponse::Rejected(PairRejectReason::InvalidSecret);
            }
            window.prompts.clone()
        };

        let (reply, decision) = oneshot::channel();
        let prompt = Prompt {
            controller,
            device_name: device_name.clone(),
            reply,
        };
        if prompts.send(prompt).await.is_err() {
            return PairResponse::Rejected(PairRejectReason::NotPairing);
        }
        match tokio::time::timeout(DECISION_TIMEOUT, decision).await {
            Ok(Ok(true)) => PairResponse::Accepted {
                device_name: self.config.name.clone(),
            },
            _ => PairResponse::Rejected(PairRejectReason::Declined),
        }
    }
}

/// Device names come from the network: limit their length and strip control characters
/// before they are shown in a terminal.
fn clean_name(name: &str) -> String {
    let name: String = name
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_string();
    if name.is_empty() {
        "Unnamed phone".into()
    } else {
        name
    }
}

/// Runs a closure when dropped.
fn scopeguard(f: impl FnOnce()) -> impl Drop {
    struct Guard<F: FnOnce()>(Option<F>);
    impl<F: FnOnce()> Drop for Guard<F> {
        fn drop(&mut self) {
            if let Some(f) = self.0.take() {
                f();
            }
        }
    }
    Guard(Some(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned() {
        assert_eq!(clean_name("  Pixel 8 "), "Pixel 8");
        assert_eq!(clean_name("evil\x1b[2Jname\n"), "evil[2Jname");
        assert_eq!(clean_name("\x07"), "Unnamed phone");
        assert_eq!(clean_name(&"x".repeat(100)).len(), 64);
    }
}
