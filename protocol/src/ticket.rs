//! Pairing tickets: what the host's QR code contains.
//!
//! Format: `rconnect://pair?id=<base32 id>&s=<base32 secret>`, about 90 characters, so the
//! QR code fits in a standard terminal. The phone finds the host by its id, through mDNS on
//! the local network or iroh's DNS address lookup. Optional `relay` and `addr` parameters
//! carry address hints.

use std::net::SocketAddr;

use url::Url;

use crate::{DeviceId, base32};

const SCHEME: &str = "rconnect";
const HOST: &str = "pair";

/// Length of the pairing secret. 80 bits cannot be guessed during a pairing window,
/// and keep the QR code small.
pub const SECRET_LEN: usize = 10;

/// One-time secret proving that the controller saw the host's QR code.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PairingSecret(pub [u8; SECRET_LEN]);

impl PairingSecret {
    pub fn generate() -> PairingSecret {
        let mut bytes = [0u8; SECRET_LEN];
        rand::fill(&mut bytes);
        PairingSecret(bytes)
    }

    /// Compares in constant time, so response timing does not leak the secret.
    pub fn matches(&self, other: &[u8; SECRET_LEN]) -> bool {
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
    /// The host's home relay, if known.
    pub relay_url: Option<String>,
    /// The host's direct addresses, if known.
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
            query.append_pair("id", &base32::encode(&self.host_id));
            query.append_pair("s", &base32::encode(&self.secret.0));
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
        if !url.scheme().eq_ignore_ascii_case(SCHEME)
            || !url
                .host_str()
                .is_some_and(|host| host.eq_ignore_ascii_case(HOST))
        {
            return Err(TicketError::WrongScheme);
        }

        let mut host_id = None;
        let mut secret = None;
        let mut relay_url = None;
        let mut direct_addrs = Vec::new();
        for (key, value) in url.query_pairs() {
            match key.to_ascii_lowercase().as_str() {
                "id" => {
                    host_id =
                        Some(base32::decode(&value).ok_or(TicketError::InvalidParameter("id"))?)
                }
                "s" => {
                    secret = Some(PairingSecret(
                        base32::decode(&value).ok_or(TicketError::InvalidParameter("s"))?,
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
            secret: secret.ok_or(TicketError::InvalidParameter("s"))?,
            relay_url,
            direct_addrs,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> PairingTicket {
        PairingTicket {
            host_id: [0xab; 32],
            secret: PairingSecret([0x01; SECRET_LEN]),
            relay_url: None,
            direct_addrs: vec![],
        }
    }

    #[test]
    fn minimal_ticket_is_short() {
        let uri = minimal().to_uri();
        assert!(uri.starts_with("rconnect://pair?id="));
        assert!(uri.len() <= 90, "{} characters: {uri}", uri.len());
        assert_eq!(PairingTicket::from_uri(&uri).unwrap(), minimal());
    }

    #[test]
    fn hints_roundtrip() {
        let ticket = PairingTicket {
            relay_url: Some("https://relay.example.org./".into()),
            direct_addrs: vec![
                "192.168.1.20:4433".parse().unwrap(),
                "[fe80::1]:4433".parse().unwrap(),
            ],
            ..minimal()
        };
        assert_eq!(PairingTicket::from_uri(&ticket.to_uri()).unwrap(), ticket);
    }

    #[test]
    fn scanners_changing_case_are_fine() {
        let uri = minimal().to_uri().to_uppercase();
        assert_eq!(PairingTicket::from_uri(&uri).unwrap(), minimal());
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
            PairingTicket::from_uri("rconnect://pair?id=00&s=00"),
            Err(TicketError::InvalidParameter("id"))
        );
        let uri = minimal().to_uri().replace("&s=", "&x=");
        assert_eq!(
            PairingTicket::from_uri(&uri),
            Err(TicketError::InvalidParameter("s"))
        );
    }

    #[test]
    fn secret_comparison() {
        let secret = PairingSecret::generate();
        assert!(secret.matches(&secret.0));
        let mut other = secret.0;
        other[SECRET_LEN - 1] ^= 1;
        assert!(!secret.matches(&other));
    }
}
