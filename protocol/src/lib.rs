//! Wire protocol shared by the RConnect host (Linux desktop) and controller (Android phone).
//!
//! See `docs/ARCHITECTURE.md` for the full design. In short:
//!
//! * Connections are iroh (QUIC) connections between two authenticated endpoint ids.
//! * [`ALPN_PAIR`] is used for pairing, [`ALPN_SESSION`] for remote control sessions.
//! * Every stream carries length-prefixed, `postcard`-encoded messages (see [`framing`]).
//! * Each stream has its own message type, defined in [`messages`].

pub mod framing;
pub mod hex;
pub mod messages;
pub mod ticket;
pub mod words;

/// Version of the session and pairing protocols. Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u16 = 1;

/// ALPN of the pairing protocol. Accepted from anyone, but only while the host has a
/// pairing window open.
pub const ALPN_PAIR: &[u8] = b"rconnect/pair/1";

/// ALPN of the session protocol. Accepted only from paired controllers.
pub const ALPN_SESSION: &[u8] = b"rconnect/session/1";

/// Raw public key of a device (the bytes of its iroh `EndpointId`).
pub type DeviceId = [u8; 32];
