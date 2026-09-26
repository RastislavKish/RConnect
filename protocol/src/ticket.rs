//! Pairing tickets: what the host's QR code contains.
//!
//! Format: `rconnect://pair?id=<hex>&secret=<hex>[&relay=<url>][&addr=<ip:port>]...`

use std::net::SocketAddr;

use url::Url;

use crate::{DeviceId, hex};

const SCHEME: &str = "rconnect";
const HOST: &str = "pair";

/// One-time secret proving that the controller saw the host's QR code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PairingSecret(pub [u8; 16]);

impl PairingSecret {
    pub fn generate() -> PairingSecret {
        let mut bytes = [0u8; 16];
        rand::fill(&mut bytes);
        PairingSecret(bytes)
    }

    /// Compares in constant time, so response timing does not leak the secret.
    pub fn matches(&self, other: &[u8; 16]) -> bool {
        self.0
            .iter()
            .zip(other)
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
    }
}

impl std::fmt::Debug for PairingSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingSecret(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingTicket {
    pub host_id: DeviceId,
    pub secret: PairingSecret,
    /// The host's home relay, so the controller can reach it without address lookup.
    pub relay_url: Option<String>,
    /// The host's direct addresses, for LAN connections.
    pub direct_addrs: Vec<SocketAddr>,
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum TicketError {
    #[error("not a valid URI")]
    InvalidUri,
    #[error("not an RConnect pairing URI")]
    WrongScheme,
    #[error("missing or invalid parameter `{0}`")]
    InvalidParameter(&'static str),
}

impl PairingTicket {
    pub fn to_uri(&self) -> String {
        let mut url = Url::parse(&format!("{SCHEME}://{HOST}")).expect("constant URI is valid");
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("id", &hex::encode(&self.host_id));
            query.append_pair("secret", &hex::encode(&self.secret.0));
            if let Some(relay) = &self.relay_url {
                query.append_pair("relay", relay);
            }
            for addr in &self.direct_addrs {
                query.append_pair("addr", &addr.to_string());
            }
        }
        url.into()
    }

    pub fn from_uri(uri: &str) -> Result<PairingTicket, TicketError> {
        let url = Url::parse(uri.trim()).map_err(|_| TicketError::InvalidUri)?;
        if url.scheme() != SCHEME || url.host_str() != Some(HOST) {
            return Err(TicketError::WrongScheme);
        }

        let mut host_id = None;
        let mut secret = None;
        let mut relay_url = None;
        let mut direct_addrs = Vec::new();
        for (key, value) in url.query_pairs() {
            match &*key {
                "id" => {
                    host_id = Some(hex::decode(&value).ok_or(TicketError::InvalidParameter("id"))?)
                }
                "secret" => {
                    secret = Some(PairingSecret(
                        hex::decode(&value).ok_or(TicketError::InvalidParameter("secret"))?,
                    ))
                }
                "relay" => relay_url = Some(value.into_owned()),
                "addr" => direct_addrs.push(
                    value
                        .parse()
                        .map_err(|_| TicketError::InvalidParameter("addr"))?,
                ),
                // Parameters added by newer versions are ignored.
                _ => {}
            }
        }

        Ok(PairingTicket {
            host_id: host_id.ok_or(TicketError::InvalidParameter("id"))?,
            secret: secret.ok_or(TicketError::InvalidParameter("secret"))?,
            relay_url,
            direct_addrs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> PairingTicket {
        PairingTicket {
            host_id: [0xab; 32],
            secret: PairingSecret([0x01; 16]),
            relay_url: Some("https://relay.example.org./".into()),
            direct_addrs: vec![
                "192.168.1.20:4433".parse().unwrap(),
                "[fe80::1]:4433".parse().unwrap(),
            ],
        }
    }

    #[test]
    fn roundtrip() {
        let ticket = ticket();
        let uri = ticket.to_uri();
        assert!(uri.starts_with("rconnect://pair?id=abab"));
        assert_eq!(PairingTicket::from_uri(&uri).unwrap(), ticket);
    }

    #[test]
    fn minimal_ticket() {
        let ticket = PairingTicket {
            relay_url: None,
            direct_addrs: vec![],
            ..ticket()
        };
        assert_eq!(PairingTicket::from_uri(&ticket.to_uri()).unwrap(), ticket);
    }

    #[test]
    fn rejects_invalid_tickets() {
        assert_eq!(
            PairingTicket::from_uri("https://pair?id=00"),
            Err(TicketError::WrongScheme)
        );
        assert_eq!(
            PairingTicket::from_uri("not a uri"),
            Err(TicketError::InvalidUri)
        );
        assert_eq!(
            PairingTicket::from_uri("rconnect://pair?id=00&secret=00"),
            Err(TicketError::InvalidParameter("id"))
        );
        let uri = ticket().to_uri().replace("secret=", "x=");
        assert_eq!(
            PairingTicket::from_uri(&uri),
            Err(TicketError::InvalidParameter("secret"))
        );
    }

    #[test]
    fn secret_comparison() {
        let secret = PairingSecret::generate();
        assert!(secret.matches(&secret.0));
        let mut other = secret.0;
        other[15] ^= 1;
        assert!(!secret.matches(&other));
    }
}
