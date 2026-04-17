//! 메시지 프레이밍: `[type_len:u8][type_str][payload]`
//!
//! 레거시 `data_publisher_rust/src/serializer.rs::create_message` 와 호환.
//!
//! # 레이아웃
//! ```text
//!  0         1              1+type_len
//! ┌─────────┬──────────────┬──────────────────┐
//! │type_len │ type_str     │ payload (wire)   │
//! │  u8     │ ASCII        │ 120 or 128 bytes │
//! └─────────┴──────────────┴──────────────────┘
//! ```
//!
//! `type_len` 이 u8 이므로 메시지 타입 문자열은 최대 255 byte.
//! 실제로는 `bookticker` (10) / `trade` (5) / `webbookticker` (13) 뿐.

use thiserror::Error;

/// 프레이밍 관련 에러.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrameError {
    /// 입력이 너무 짧음 (type_len 1바이트도 없음).
    #[error("frame too short: got {0} bytes")]
    TooShort(usize),

    /// type_len 이 메시지 전체 길이를 초과.
    #[error("declared type_len {type_len} exceeds frame len {frame_len}")]
    TypeLenOverflow {
        /// 선언된 타입 길이.
        type_len: usize,
        /// 프레임 전체 길이.
        frame_len: usize,
    },

    /// 메시지 타입 문자열이 255 초과 (u8 오버플로).
    #[error("message type too long: {0} bytes (max 255)")]
    TypeStringTooLong(usize),

    /// 타입 문자열이 UTF-8 아님.
    #[error("non-utf8 bytes in message type")]
    InvalidTypeUtf8,
}

/// 프레임을 생성 — payload 를 새 `Vec<u8>` 에 복사.
///
/// hot path 에서는 재사용 버퍼 쓰는 `write_frame_into` 권장.
pub fn create_message(msg_type: &str, payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    let type_bytes = msg_type.as_bytes();
    if type_bytes.len() > u8::MAX as usize {
        return Err(FrameError::TypeStringTooLong(type_bytes.len()));
    }

    let mut msg = Vec::with_capacity(1 + type_bytes.len() + payload.len());
    msg.push(type_bytes.len() as u8);
    msg.extend_from_slice(type_bytes);
    msg.extend_from_slice(payload);
    Ok(msg)
}

/// 프레임을 기존 버퍼에 써 넣음. 버퍼는 미리 resize/clear 되어 있어야 함.
/// 버퍼를 확장해 정확한 크기로 만든다.
pub fn write_frame_into(
    buf: &mut Vec<u8>,
    msg_type: &str,
    payload: &[u8],
) -> Result<(), FrameError> {
    let type_bytes = msg_type.as_bytes();
    if type_bytes.len() > u8::MAX as usize {
        return Err(FrameError::TypeStringTooLong(type_bytes.len()));
    }
    buf.clear();
    buf.reserve(1 + type_bytes.len() + payload.len());
    buf.push(type_bytes.len() as u8);
    buf.extend_from_slice(type_bytes);
    buf.extend_from_slice(payload);
    Ok(())
}

/// 파싱된 프레임: (msg_type, payload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameView<'a> {
    /// 메시지 타입 문자열.
    pub msg_type: &'a str,
    /// payload (zero-copy slice).
    pub payload: &'a [u8],
}

/// 프레임을 zero-copy 로 파싱.
pub fn parse_frame(frame: &[u8]) -> Result<FrameView<'_>, FrameError> {
    if frame.is_empty() {
        return Err(FrameError::TooShort(0));
    }
    let type_len = frame[0] as usize;
    if 1 + type_len > frame.len() {
        return Err(FrameError::TypeLenOverflow {
            type_len,
            frame_len: frame.len(),
        });
    }
    let type_bytes = &frame[1..1 + type_len];
    let msg_type = std::str::from_utf8(type_bytes).map_err(|_| FrameError::InvalidTypeUtf8)?;
    let payload = &frame[1 + type_len..];
    Ok(FrameView { msg_type, payload })
}

// ─────────────────────────────────────────────────────────────────────────────
// 테스트
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_bookticker_frame() {
        let payload = vec![0xAB; 120];
        let frame = create_message("bookticker", &payload).unwrap();
        assert_eq!(frame[0], 10);
        assert_eq!(&frame[1..11], b"bookticker");
        assert_eq!(&frame[11..], &payload[..]);

        let v = parse_frame(&frame).unwrap();
        assert_eq!(v.msg_type, "bookticker");
        assert_eq!(v.payload, &payload[..]);
    }

    #[test]
    fn write_frame_into_matches_create() {
        let payload = vec![0x11; 64];
        let expected = create_message("trade", &payload).unwrap();
        let mut buf = Vec::with_capacity(128);
        write_frame_into(&mut buf, "trade", &payload).unwrap();
        assert_eq!(buf, expected);
    }

    #[test]
    fn parse_empty_frame_fails() {
        assert_eq!(parse_frame(&[]), Err(FrameError::TooShort(0)));
    }

    #[test]
    fn parse_type_len_overflow_fails() {
        // type_len = 10, but only 5 bytes follow
        let frame = b"\x0Ahello";
        match parse_frame(frame) {
            Err(FrameError::TypeLenOverflow {
                type_len: 10,
                frame_len: 6,
            }) => {}
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn create_rejects_over_255_type() {
        let long = "a".repeat(300);
        assert!(matches!(
            create_message(&long, b""),
            Err(FrameError::TypeStringTooLong(300))
        ));
    }

    #[test]
    fn parse_invalid_utf8_type() {
        // type_len=2, bytes 0xFF 0xFE are invalid utf8
        let frame = [2u8, 0xFF, 0xFE, 0x00];
        assert_eq!(parse_frame(&frame), Err(FrameError::InvalidTypeUtf8));
    }

    #[test]
    fn empty_payload_is_allowed() {
        let f = create_message("trade", &[]).unwrap();
        let v = parse_frame(&f).unwrap();
        assert_eq!(v.msg_type, "trade");
        assert!(v.payload.is_empty());
    }
}
