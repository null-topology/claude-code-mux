use bytes::{Bytes, BytesMut};

// Connect frame flags
pub const FLAG_GZIP: u8 = 0x01;
pub const FLAG_END: u8 = 0x02;

/// A single Connect frame with flags and payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFrame {
    pub flags: u8,
    pub payload: Bytes,
}

/// Encode a payload into a Connect frame: 1 byte flags, 4 byte big-endian
/// payload length, then the payload bytes.
pub fn encode_connect_frame(payload: impl AsRef<[u8]>, flags: u8) -> Bytes {
    let payload = payload.as_ref();
    let mut out = BytesMut::with_capacity(5 + payload.len());
    out.extend_from_slice(&[flags]);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out.freeze()
}

/// Streaming decoder for Connect frames from a byte source.
///
/// Handles split chunks, multiple frames in a single chunk, and malformed
/// (oversized) lengths. Does NOT handle gzip decompression inline -- the
/// caller checks `FLAG_GZIP` and decompresses if desired.
///
/// End frames (FLAG_END set) with an empty or JSON payload are returned
/// as ConnectFrames. The caller inspects the payload to determine whether
/// it conveys a Connect error.
#[derive(Default)]
pub struct ConnectFrameDecoder {
    buffer: BytesMut,
}

impl ConnectFrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes into the decoder. Returns all complete frames found.
    ///
    /// Returns an error if a frame header advertises a length that exceeds
    /// `max_frame_payload` (default 64 MiB).
    pub fn push(&mut self, chunk: impl AsRef<[u8]>) -> Result<Vec<ConnectFrame>, ConnectError> {
        self.buffer.extend_from_slice(chunk.as_ref());
        self.drain(64 * 1024 * 1024) // 64 MiB max payload
    }

    /// Same as `push` but with an explicit `max_payload` limit for testing.
    pub fn push_with_limit(
        &mut self,
        chunk: impl AsRef<[u8]>,
        max_payload: usize,
    ) -> Result<Vec<ConnectFrame>, ConnectError> {
        self.buffer.extend_from_slice(chunk.as_ref());
        self.drain(max_payload)
    }

    fn drain(&mut self, max_payload: usize) -> Result<Vec<ConnectFrame>, ConnectError> {
        let mut out = Vec::new();
        loop {
            if self.buffer.len() < 5 {
                break;
            }
            let len = u32::from_be_bytes([
                self.buffer[1],
                self.buffer[2],
                self.buffer[3],
                self.buffer[4],
            ]) as usize;

            if len > max_payload {
                return Err(ConnectError::PayloadTooLarge {
                    length: len,
                    max: max_payload,
                });
            }

            if self.buffer.len() < 5 + len {
                break;
            }

            let mut raw = self.buffer.split_to(5 + len);
            out.push(ConnectFrame {
                flags: raw[0],
                payload: raw.split_off(5).freeze(),
            });
        }
        Ok(out)
    }

    /// Return the number of buffered bytes (incomplete frame data).
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

/// The most one gzip frame is inflated to.
pub const MAX_GZIP_FRAME_BYTES: usize = 32 * 1024 * 1024;
/// The most the gzip frames of one response are inflated to together.
pub const MAX_GZIP_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

/// Decode gzipped payload bytes, refusing a frame that inflates past
/// `MAX_GZIP_FRAME_BYTES`. The caller decides when to call this based on
/// frame flags & FLAG_GZIP.
pub fn decode_gzip_frame(payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
    GzipFrames::new().decode(payload)
}

/// Inflates the gzip frames of one response, each up to a frame limit and
/// all of them together up to a response limit. Inflation stops one byte
/// past the smaller of the two, so no more than that is ever held.
pub struct GzipFrames {
    frame_limit: usize,
    response_limit: usize,
    left: usize,
}

impl GzipFrames {
    pub fn new() -> Self {
        Self::with_limits(MAX_GZIP_FRAME_BYTES, MAX_GZIP_RESPONSE_BYTES)
    }

    fn with_limits(frame_limit: usize, response_limit: usize) -> Self {
        Self {
            frame_limit,
            response_limit,
            left: response_limit,
        }
    }

    pub fn decode(&mut self, payload: &[u8]) -> Result<Vec<u8>, std::io::Error> {
        use std::io::Read;
        let limit = self.frame_limit.min(self.left);
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(payload)
            .take(limit as u64 + 1)
            .read_to_end(&mut out)?;
        let refused = if out.len() > self.frame_limit {
            format!("gzip frame inflates past {} bytes", self.frame_limit)
        } else if out.len() > self.left {
            format!(
                "gzip frames of one response inflate past {} bytes",
                self.response_limit
            )
        } else {
            self.left -= out.len();
            return Ok(out);
        };
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            refused,
        ))
    }
}

impl Default for GzipFrames {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a Connect end-frame JSON error payload into a structured error.
///
/// Returns `None` if the payload is empty or not valid Connect error JSON.
pub fn parse_connect_error(payload: &[u8]) -> Option<ConnectEndError> {
    if payload.is_empty() {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_slice(payload).ok()?;
    let error = parsed.get("error")?;
    let code = error.get("code")?.as_str()?;
    let message = error.get("message")?.as_str().unwrap_or("Connect error");
    let status = match code {
        "resource_exhausted" => 429,
        _ => 502,
    };
    Some(ConnectEndError {
        code: code.to_string(),
        message: message.to_string(),
        detail: parsed.to_string(),
        status,
    })
}

#[derive(Debug, Clone)]
pub struct ConnectEndError {
    pub code: String,
    pub message: String,
    pub detail: String,
    pub status: u16,
}

impl std::fmt::Display for ConnectEndError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Connect error {}: {} ({})",
            self.status, self.message, self.code
        )
    }
}

impl std::error::Error for ConnectEndError {}

#[derive(Debug, Clone)]
pub enum ConnectError {
    PayloadTooLarge { length: usize, max: usize },
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::PayloadTooLarge { length, max } => {
                write!(f, "Connect frame payload {length} exceeds max {max}")
            }
        }
    }
}

impl std::error::Error for ConnectError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_roundtrip() {
        let frame = encode_connect_frame(b"hello", 0);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, 0);
        assert_eq!(&frames[0].payload[..], b"hello");
    }

    #[test]
    fn encode_with_gzip_flag() {
        let frame = encode_connect_frame(b"gzip-data", FLAG_GZIP);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP);
    }

    #[test]
    fn encode_with_end_flag() {
        let frame = encode_connect_frame(b"", FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_END);
        assert!(frames[0].payload.is_empty());
    }

    #[test]
    fn encode_with_gzip_and_end_flags() {
        let payload = b"end-data";
        let frame = encode_connect_frame(payload, FLAG_GZIP | FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(&frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP | FLAG_END);
        assert_eq!(&frames[0].payload[..], payload);
    }

    #[test]
    fn multiple_frames_in_single_chunk() {
        let f1 = encode_connect_frame(b"first", 0);
        let f2 = encode_connect_frame(b"second", 0);
        let mut combined = BytesMut::new();
        combined.extend_from_slice(&f1);
        combined.extend_from_slice(&f2);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(combined).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(&frames[0].payload[..], b"first");
        assert_eq!(&frames[1].payload[..], b"second");
    }

    #[test]
    fn split_chunks_are_assembled() {
        let frame = encode_connect_frame(b"split-test", 0);
        let (a, b) = frame.split_at(3);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(a).unwrap();
        assert!(frames.is_empty());

        let frames = decoder.push(b).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].payload[..], b"split-test");
    }

    #[test]
    fn split_at_header_boundary() {
        let frame = encode_connect_frame(b"split-at-5", 0);
        // Split after the flags byte but before the length bytes are complete
        let (a, b) = frame.split_at(1);

        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(a).unwrap();
        assert!(frames.is_empty());

        let frames = decoder.push(b).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(&frames[0].payload[..], b"split-at-5");
    }

    #[test]
    fn oversized_length_is_rejected() {
        let mut decoder = ConnectFrameDecoder::new();
        // Encode a frame with 1M payload (will exceed our 10-byte max)
        let oversized = encode_connect_frame(vec![0u8; 100], 0);
        let result = decoder.push_with_limit(&oversized, 10);
        assert!(result.is_err());
        match result.unwrap_err() {
            ConnectError::PayloadTooLarge { length, max } => {
                assert_eq!(length, 100);
                assert_eq!(max, 10);
            }
        }
    }

    #[test]
    fn empty_chunk_produces_no_frames() {
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(b"").unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn buf_returns_buffered_bytes() {
        let mut decoder = ConnectFrameDecoder::new();
        // Push part of a frame header
        decoder.push(b"\x00\x00").unwrap();
        assert_eq!(decoder.buffered(), 2);
    }

    #[test]
    fn clean_end_frame_empty_payload() {
        let frame = encode_connect_frame(b"", FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_END);
        assert!(frames[0].payload.is_empty());
        // Parse error from empty payload
        assert!(parse_connect_error(&frames[0].payload).is_none());
    }

    #[test]
    fn connect_json_error_parsing() {
        let json_err = serde_json::json!({
            "error": {
                "code": "resource_exhausted",
                "message": "quota exceeded",
                "details": []
            }
        });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let frame = encode_connect_frame(&payload, FLAG_END);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);

        let err = parse_connect_error(&frames[0].payload).unwrap();
        assert_eq!(err.code, "resource_exhausted");
        assert_eq!(err.status, 429);
        assert_eq!(err.message, "quota exceeded");
    }

    #[test]
    fn connect_json_unavailable_error() {
        let json_err = serde_json::json!({
            "error": {
                "code": "unavailable",
                "message": "service unavailable"
            }
        });
        let payload = serde_json::to_vec(&json_err).unwrap();
        let err = parse_connect_error(&payload).unwrap();
        assert_eq!(err.code, "unavailable");
        assert_eq!(err.status, 502);
    }

    #[test]
    fn frame_fixture_matches_reference_layout() {
        // Connect frame: flags=0x00, length=3 (0x00000003), payload="abc"
        // Wire format: [0x00, 0x00, 0x00, 0x00, 0x03, 0x61, 0x62, 0x63]
        let frame = encode_connect_frame(b"abc", 0);
        assert_eq!(hex::encode(frame), "0000000003616263");
    }

    #[test]
    fn frame_fixture_with_flags() {
        // flags=0x01, length=3
        let frame = encode_connect_frame(b"xyz", 0x01);
        assert_eq!(hex::encode(frame), "010000000378797a");
    }

    #[test]
    fn gzip_frame_decompress() {
        let payload = b"hello gzip";
        let mut compressed = Vec::new();
        {
            use std::io::Write;
            let mut encoder =
                flate2::write::GzEncoder::new(&mut compressed, flate2::Compression::fast());
            encoder.write_all(payload).unwrap();
            encoder.finish().unwrap();
        }

        let frame = encode_connect_frame(&compressed, FLAG_GZIP);
        let mut decoder = ConnectFrameDecoder::new();
        let frames = decoder.push(frame).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].flags, FLAG_GZIP);

        let decompressed = decode_gzip_frame(&frames[0].payload).unwrap();
        assert_eq!(decompressed, b"hello gzip");
    }

    fn gzip(payload: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(payload).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn a_gzip_frame_inflates_up_to_the_frame_limit() {
        let limit = 64 * 1024;
        let at_limit = gzip(&vec![0; limit]);
        let past_limit = gzip(&vec![0; 16 * limit]);
        assert!(past_limit.len() < 2048, "{} bytes", past_limit.len());

        let mut frames = GzipFrames::with_limits(limit, 4 * limit);
        assert_eq!(frames.decode(&at_limit).unwrap().len(), limit);
        let error = frames.decode(&past_limit).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "gzip frame inflates past 65536 bytes");
    }

    #[test]
    fn the_gzip_frames_of_a_response_share_one_limit() {
        let mut frames = GzipFrames::with_limits(8, 12);
        assert_eq!(frames.decode(&gzip(b"12345678")).unwrap().len(), 8);
        assert_eq!(frames.decode(&gzip(b"1234")).unwrap().len(), 4);
        assert_eq!(frames.decode(&gzip(b"")).unwrap().len(), 0);
        let error = frames.decode(&gzip(b"1")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "gzip frames of one response inflate past 12 bytes"
        );
    }
}
