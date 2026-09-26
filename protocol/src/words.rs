//! Human readable verification words.
//!
//! Hashes are turned into words from the 2048-word BIP39 English list, 11 bits per word.
//! The words are meant to be compared by ear, through a screen reader.

use crate::DeviceId;

/// Number of words shown when pairing (44 bits).
pub const PAIRING_WORD_COUNT: usize = 4;

/// Number of words in a device fingerprint (66 bits).
pub const FINGERPRINT_WORD_COUNT: usize = 6;

const PAIRING_CONTEXT: &str = "RConnect 2026-09-27 pairing verification words v1";
const FINGERPRINT_CONTEXT: &str = "RConnect 2026-09-27 device fingerprint words v1";

/// Words both devices show during pairing. Matching words prove that the host and the
/// controller see each other's real identities.
pub fn pairing_words(host: &DeviceId, controller: &DeviceId) -> [&'static str; PAIRING_WORD_COUNT] {
    let mut input = [0u8; 64];
    input[..32].copy_from_slice(host);
    input[32..].copy_from_slice(controller);
    words_from_hash(&blake3::derive_key(PAIRING_CONTEXT, &input))
}

/// Fingerprint of a single device, shown in device details to re-check a pairing.
pub fn fingerprint_words(id: &DeviceId) -> [&'static str; FINGERPRINT_WORD_COUNT] {
    words_from_hash(&blake3::derive_key(FINGERPRINT_CONTEXT, id))
}

/// Takes consecutive 11-bit groups from the start of `hash`, most significant bit first.
fn words_from_hash<const N: usize>(hash: &[u8; 32]) -> [&'static str; N] {
    const { assert!(N * 11 <= 256) };
    let list = bip39::Language::English.word_list();
    std::array::from_fn(|word| {
        let mut index = 0usize;
        for bit in word * 11..(word + 1) * 11 {
            let set = hash[bit / 8] >> (7 - bit % 8) & 1;
            index = (index << 1) | set as usize;
        }
        list[index]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_eleven_bit_groups() {
        let mut hash = [0u8; 32];
        // First word: index 0b000_0000_0001 = 1, second: index 0b111_1111_1111 = 2047.
        hash[0] = 0b0000_0000;
        hash[1] = 0b0011_1111;
        hash[2] = 0b1111_1100;
        let words: [&str; 2] = words_from_hash(&hash);
        let list = bip39::Language::English.word_list();
        assert_eq!(words, [list[1], list[2047]]);
    }

    #[test]
    fn pairing_words_depend_on_roles() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_eq!(pairing_words(&a, &b), pairing_words(&a, &b));
        assert_ne!(pairing_words(&a, &b), pairing_words(&b, &a));
        assert_ne!(pairing_words(&a, &b), pairing_words(&a, &[3u8; 32]));
    }

    #[test]
    fn fingerprints_differ() {
        assert_ne!(fingerprint_words(&[1u8; 32]), fingerprint_words(&[2u8; 32]));
    }
}
