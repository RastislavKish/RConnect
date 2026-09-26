//! Framing of messages on QUIC streams.
//!
//! A frame is a LEB128 varint length followed by that many bytes of a `postcard`-encoded
//! message. Because the length is read before decoding, a message that fails to decode
//! (for example an enum variant added by a newer peer) is consumed completely and the
//! stream stays usable: [`read_message`] returns [`Error::Decode`] and the caller may
//! keep reading.
//!
//! [`read_message`] reads the length byte by byte, so callers should wrap unbuffered
//! streams in a [`tokio::io::BufReader`].

use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Largest frame accepted by [`read_message`]. Speech messages carry at most a few
/// paragraphs, so anything above this is treated as a protocol violation.
pub const MAX_FRAME_LEN: usize = 1 << 20;

/// Longest possible LEB128 encoding of a `u32`.
const MAX_VARINT_LEN: usize = 5;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to encode message: {0}")]
    Encode(postcard::Error),
    #[error("failed to decode message: {0}")]
    Decode(postcard::Error),
    #[error("frame of {0} bytes exceeds the limit")]
    FrameTooLarge(usize),
    #[error("invalid frame length prefix")]
    InvalidLength,
    #[error("stream ended in the middle of a frame")]
    UnexpectedEof,
}

impl Error {
    /// Whether the stream is still aligned on a frame boundary after this error, so
    /// reading can continue.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Error::Decode(_))
    }
}

/// Encodes a message into a complete frame (length prefix included).
pub fn encode<T: Serialize>(message: &T) -> Result<Vec<u8>, Error> {
    let body = postcard::to_allocvec(message).map_err(Error::Encode)?;
    if body.len() > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge(body.len()));
    }
    let mut frame = Vec::with_capacity(body.len() + MAX_VARINT_LEN);
    let mut len = body.len() as u32;
    loop {
        let byte = (len & 0x7f) as u8;
        len >>= 7;
        if len == 0 {
            frame.push(byte);
            break;
        }
        frame.push(byte | 0x80);
    }
    frame.extend_from_slice(&body);
    Ok(frame)
}

/// Encodes a message and writes it as one frame.
pub async fn write_message<W, T>(writer: &mut W, message: &T) -> Result<(), Error>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    writer.write_all(&encode(message)?).await?;
    Ok(())
}

/// Reads one frame and decodes it.
///
/// Returns `Ok(None)` when the stream ends cleanly on a frame boundary.
pub async fn read_message<R, T>(reader: &mut R) -> Result<Option<T>, Error>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let Some(len) = read_length(reader).await? else {
        return Ok(None);
    };
    if len > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge(len));
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).await.map_err(eof_in_frame)?;
    postcard::from_bytes(&body).map(Some).map_err(Error::Decode)
}

async fn read_length<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<usize>, Error> {
    let mut value: u32 = 0;
    for i in 0..MAX_VARINT_LEN {
        let byte = match reader.read_u8().await {
            Ok(byte) => byte,
            Err(e) if i == 0 && e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(eof_in_frame(e)),
        };
        let bits = (byte & 0x7f) as u32;
        if i == MAX_VARINT_LEN - 1 && bits > 0x0f {
            return Err(Error::InvalidLength);
        }
        value |= bits << (7 * i);
        if byte & 0x80 == 0 {
            return Ok(Some(value as usize));
        }
    }
    Err(Error::InvalidLength)
}

fn eof_in_frame(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Error::UnexpectedEof
    } else {
        Error::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{KeyMessage, Mark, SpeechCommand, SpeechKind, Utterance};

    #[tokio::test]
    async fn roundtrip_several_messages() {
        let messages = vec![
            KeyMessage::Down(30),
            KeyMessage::Up(30),
            KeyMessage::Down(1000),
            KeyMessage::ReleaseAll,
        ];
        let mut buffer = Vec::new();
        for message in &messages {
            write_message(&mut buffer, message).await.unwrap();
        }

        let mut reader = buffer.as_slice();
        for expected in &messages {
            let message: KeyMessage = read_message(&mut reader).await.unwrap().unwrap();
            assert_eq!(&message, expected);
        }
        assert!(
            read_message::<_, KeyMessage>(&mut reader)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn key_frames_are_small() {
        assert_eq!(encode(&KeyMessage::Down(30)).unwrap().len(), 3);
        assert_eq!(encode(&KeyMessage::ReleaseAll).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn long_frame_uses_multibyte_length() {
        let message = SpeechCommand::Speak(Utterance {
            id: 7,
            kind: SpeechKind::Text,
            text: "a".repeat(300),
            marks: vec![Mark {
                offset: 0,
                index: 0,
            }],
        });
        let frame = encode(&message).unwrap();
        assert!(frame[0] & 0x80 != 0);
        let decoded: SpeechCommand = read_message(&mut frame.as_slice()).await.unwrap().unwrap();
        assert_eq!(decoded, message);
    }

    #[tokio::test]
    async fn stream_stays_aligned_after_unknown_variant() {
        // Variant tag 200 does not exist in KeyMessage.
        let mut buffer = vec![1, 200];
        buffer.extend(encode(&KeyMessage::Down(2)).unwrap());

        let mut reader = buffer.as_slice();
        let error = read_message::<_, KeyMessage>(&mut reader)
            .await
            .unwrap_err();
        assert!(error.is_recoverable());
        let next: KeyMessage = read_message(&mut reader).await.unwrap().unwrap();
        assert_eq!(next, KeyMessage::Down(2));
    }

    #[tokio::test]
    async fn truncated_frame_is_an_error() {
        let frame = encode(&KeyMessage::Down(30)).unwrap();
        let mut reader = &frame[..frame.len() - 1];
        let error = read_message::<_, KeyMessage>(&mut reader)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::UnexpectedEof));
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected() {
        // Length prefix of 2^21.
        let buffer = [0x80, 0x80, 0x80, 0x01];
        let error = read_message::<_, KeyMessage>(&mut buffer.as_slice())
            .await
            .unwrap_err();
        assert!(matches!(error, Error::FrameTooLarge(_)));
    }
}
