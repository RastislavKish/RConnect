//! Audio datagrams.
//!
//! Each QUIC datagram carries one Opus frame after a fixed 7-byte header:
//!
//! | Bytes | Field |
//! |---|---|
//! | 0–1 | `seq`, big endian, increases by one per frame and wraps |
//! | 2–5 | `timestamp` of the frame's first sample, in samples at the stream's rate, wraps |
//! | 6 | flags ([`AudioPacket::end_of_burst`] is bit 0) |
//! | 7… | Opus payload |
//!
//! Late datagrams are dropped, never retransmitted.

/// Length of the header in front of the payload.
pub const HEADER_LEN: usize = 7;

const FLAG_END_OF_BURST: u8 = 1 << 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPacket {
    pub seq: u16,
    pub timestamp: u32,
    /// The last frame before the host stops sending because nothing audible plays. The
    /// controller fades out instead of concealing a loss.
    pub end_of_burst: bool,
    pub payload: Vec<u8>,
}

impl AudioPacket {
    pub fn encode(&self) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(HEADER_LEN + self.payload.len());
        bytes.extend_from_slice(&self.seq.to_be_bytes());
        bytes.extend_from_slice(&self.timestamp.to_be_bytes());
        bytes.push(if self.end_of_burst {
            FLAG_END_OF_BURST
        } else {
            0
        });
        bytes.extend_from_slice(&self.payload);
        bytes
    }

    /// Returns `None` for a datagram too short to hold the header.
    pub fn decode(bytes: &[u8]) -> Option<AudioPacket> {
        if bytes.len() < HEADER_LEN {
            return None;
        }
        Some(AudioPacket {
            seq: u16::from_be_bytes([bytes[0], bytes[1]]),
            timestamp: u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]),
            end_of_burst: bytes[6] & FLAG_END_OF_BURST != 0,
            payload: bytes[HEADER_LEN..].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let packet = AudioPacket {
            seq: 0xfffe,
            timestamp: 0x1234_5678,
            end_of_burst: true,
            payload: vec![1, 2, 3],
        };
        let bytes = packet.encode();
        assert_eq!(bytes.len(), HEADER_LEN + 3);
        assert_eq!(AudioPacket::decode(&bytes), Some(packet));
        assert_eq!(AudioPacket::decode(&bytes[..6]), None);
    }
}
