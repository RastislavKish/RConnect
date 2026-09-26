//! Minimal lowercase hex encoding, used for ids and secrets in pairing tickets.

pub fn encode(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Decodes a hex string (either case) into exactly `N` bytes.
pub fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.as_bytes();
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, pair) in text.as_chunks::<2>().0.iter().enumerate() {
        out[i] = (digit(pair[0])? << 4) | digit(pair[1])?;
    }
    Some(out)
}

fn digit(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let bytes = [0x00, 0x7f, 0x80, 0xff, 0x12];
        let text = encode(&bytes);
        assert_eq!(text, "007f80ff12");
        assert_eq!(decode::<5>(&text), Some(bytes));
        assert_eq!(decode::<5>("007F80FF12"), Some(bytes));
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(decode::<2>("abc"), None);
        assert_eq!(decode::<2>("abcg"), None);
        assert_eq!(decode::<1>("abcd"), None);
    }
}
