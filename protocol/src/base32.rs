//! RFC 4648 base32 without padding, in lowercase. Used in pairing tickets, where it is
//! shorter than hex and survives case changes by QR code scanners.

const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(5) * 8);
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for &byte in bytes {
        buffer = (buffer << 8) | byte as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Decodes base32 (either case) into exactly `N` bytes.
pub fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != (N * 8).div_ceil(5) {
        return None;
    }
    let mut out = [0u8; N];
    let mut buffer: u32 = 0;
    let mut bits = 0;
    let mut index = 0;
    for c in text.bytes() {
        let value = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buffer = (buffer << 5) | value as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out[index] = (buffer >> bits) as u8;
            index += 1;
        }
    }
    // Leftover bits must be zero padding, so every string maps to one value.
    if buffer & ((1 << bits) - 1) != 0 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors() {
        assert_eq!(encode(b"f"), "my");
        assert_eq!(encode(b"fo"), "mzxq");
        assert_eq!(encode(b"foo"), "mzxw6");
        assert_eq!(encode(b"foob"), "mzxw6yq");
        assert_eq!(encode(b"fooba"), "mzxw6ytb");
        assert_eq!(encode(b"foobar"), "mzxw6ytboi");
        assert_eq!(decode::<6>("MZXW6YTBOI"), Some(*b"foobar"));
    }

    #[test]
    fn roundtrip_and_rejections() {
        let bytes: [u8; 32] = std::array::from_fn(|i| (i * 37 + 11) as u8);
        let text = encode(&bytes);
        assert_eq!(text.len(), 52);
        assert_eq!(decode::<32>(&text), Some(bytes));
        assert_eq!(decode::<32>(&text[1..]), None);
        assert_eq!(decode::<1>("m1"), None);
        // "mz" has non-zero padding bits for one byte.
        assert_eq!(decode::<1>("mz"), None);
    }
}
