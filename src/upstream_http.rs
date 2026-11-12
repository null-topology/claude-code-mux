use std::fmt;
use std::time::{Duration, Instant};

/// The `Display` text of `error` with its URL removed. A reqwest error that
/// holds a URL ends its text with ` for url (<url>)`, and an upstream URL can
/// carry a credential in its path or query, so the text that reaches a client,
/// the CLI or `proxy.log` must never include it. `Debug` and the source chain
/// are left out for the same reason.
pub(crate) fn error_text(error: reqwest::Error) -> String {
    error.without_url().to_string()
}

/// Why [`read_body_capped`] returned no body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BodyReadError {
    /// The body is longer than `limit` bytes, as declared or as read.
    TooLarge { limit: usize },
    /// Reading the body failed. The text comes from [`error_text`], so it
    /// holds no URL.
    Transport(String),
}

impl fmt::Display for BodyReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { limit } => write!(f, "response body exceeded {limit} bytes"),
            Self::Transport(text) => f.write_str(text),
        }
    }
}

impl std::error::Error for BodyReadError {}

/// The body of `response`, at most `limit` bytes of it. A declared length over
/// `limit` is refused before anything is read; otherwise the body is read
/// chunk by chunk and refused as soon as it would pass `limit`, so an
/// oversized or endless body never sits in memory whole. A body of exactly
/// `limit` bytes is returned. A read failure carries URL-free text.
pub(crate) async fn read_body_capped(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, BodyReadError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyReadError::TooLarge { limit });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| BodyReadError::Transport(error_text(error)))?
    {
        if chunk.len() > limit - body.len() {
            return Err(BodyReadError::TooLarge { limit });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// The failure of a blocking body read that ran out of time.
pub(crate) const BODY_READ_TIMEOUT_TEXT: &str = "timed out reading the response body";

/// [`read_body_capped`] for a response of the blocking client: a declared
/// length over `limit` is refused before anything is read; otherwise the body
/// is read piece by piece and refused as soon as it would pass `limit`.
///
/// The whole read is bounded by `deadline`, which the caller sets to the
/// timeout of the client that sent the request, as reqwest's own `bytes()`
/// and `json()` bound it. The blocking `Read` impl starts that timeout again
/// on every read, so a body that keeps arriving a byte at a time would
/// otherwise never time out. A read in progress cannot be interrupted: no new
/// read starts once `deadline` has passed, and the call then fails with
/// [`BODY_READ_TIMEOUT_TEXT`], so it returns within `deadline` plus at most
/// one per-read timeout.
pub(crate) fn read_blocking_body_capped(
    mut response: reqwest::blocking::Response,
    limit: usize,
    deadline: Duration,
) -> Result<Vec<u8>, BodyReadError> {
    use std::io::Read;

    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyReadError::TooLarge { limit });
    }
    let started = Instant::now();
    let mut body = Vec::new();
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if started.elapsed() >= deadline {
            return Err(BodyReadError::Transport(BODY_READ_TIMEOUT_TEXT.to_string()));
        }
        let read = match response.read(&mut buffer) {
            Ok(0) => return Ok(body),
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(blocking_read_error(error)),
        };
        if read > limit - body.len() {
            return Err(BodyReadError::TooLarge { limit });
        }
        body.extend_from_slice(&buffer[..read]);
    }
}

/// The blocking client reports a failed body read as an `io::Error` wrapping
/// the `reqwest::Error`, whose text may hold the URL.
fn blocking_read_error(error: std::io::Error) -> BodyReadError {
    let kind = error.kind();
    match error.into_inner() {
        Some(inner) => match inner.downcast::<reqwest::Error>() {
            Ok(error) => BodyReadError::Transport(error_text(*error)),
            Err(other) => BodyReadError::Transport(other.to_string()),
        },
        None => BodyReadError::Transport(kind.to_string()),
    }
}

/// [`error_text`] as an `anyhow::Error`, for a function that propagates a
/// failed send with `?`.
pub(crate) fn url_free_error(error: reqwest::Error) -> anyhow::Error {
    anyhow::anyhow!(error_text(error))
}

/// `body` parsed as JSON. A body that does not parse fails as reqwest's
/// `json()` does, with "error decoding response body" and the parse error as
/// its source, and with no URL.
pub(crate) fn decode_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> anyhow::Result<T> {
    serde_json::from_slice(body)
        .map_err(|error| anyhow::Error::new(error).context("error decoding response body"))
}

/// A JSON answer of the async client, read under `limit` with
/// [`read_body_capped`] and parsed with [`decode_json`].
pub(crate) async fn read_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
    limit: usize,
) -> anyhow::Result<T> {
    decode_json(&read_body_capped(response, limit).await?)
}

/// A JSON answer of the blocking client, read under `limit` and `deadline`
/// with [`read_blocking_body_capped`] and parsed with [`decode_json`].
pub(crate) fn read_blocking_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
    limit: usize,
    deadline: Duration,
) -> anyhow::Result<T> {
    decode_json(&read_blocking_body_capped(response, limit, deadline)?)
}

/// A body of the blocking client as text for an error message, empty when it
/// cannot be read within `deadline` or is over `limit`, as reqwest's
/// `text().unwrap_or_default()` gives for a body it cannot read.
pub(crate) fn blocking_error_text(
    response: reqwest::blocking::Response,
    limit: usize,
    deadline: Duration,
) -> String {
    read_blocking_body_capped(response, limit, deadline)
        .map(|body| String::from_utf8_lossy(&body).into_owned())
        .unwrap_or_default()
}

/// `text` with every control character (`char::is_control`) replaced by its
/// `escape_default` form, so an upstream-provided string printed to a
/// terminal cannot move the cursor, rewrite the line or send an escape
/// sequence.
pub(crate) fn escape_control_chars(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

/// Loopback servers for the provider tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// A URL on a loopback listener that stays bound and closes every
    /// connection without answering, so a request to it fails while it is
    /// being sent. Its path and query hold a toy secret.
    pub(crate) fn unanswered_secret_url() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || close_unanswered(listener));
        format!("http://{addr}/toy-secret-path?token=toy-secret-query")
    }

    /// Reads each request on `listener` and closes its connection without an
    /// answer, for as long as the test process runs.
    fn close_unanswered(listener: TcpListener) {
        for mut stream in listener.incoming().flatten() {
            read_request(&mut stream);
        }
    }

    /// Asserts that `text` names neither the toy secret nor the address.
    pub(crate) fn assert_no_url(text: &str) {
        assert!(!text.is_empty());
        for leaked in [
            "toy-secret-path",
            "toy-secret-query",
            "127.0.0.1",
            "for url",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text:?}");
        }
    }

    /// A body of exactly `size` bytes: `json` followed by spaces.
    pub(crate) fn padded_json(json: &str, size: usize) -> Vec<u8> {
        assert!(json.len() <= size);
        let mut body = json.as_bytes().to_vec();
        body.resize(size, b' ');
        body
    }

    /// Answers one connection per entry of `responses`, in order, each with
    /// its status and body, a declared `content-length` and `connection:
    /// close`, then closes every later connection without answering, so a
    /// later request fails while it is being sent. Returns
    /// `http://<addr>/toy-secret-path`.
    pub(crate) fn serve_in_order(responses: Vec<(u16, Vec<u8>)>) -> String {
        serve(responses, true)
    }

    /// [`serve_in_order`] with no declared length: each body ends when its
    /// connection closes, so a reader learns its size only by reading it.
    pub(crate) fn serve_in_order_undeclared(responses: Vec<(u16, Vec<u8>)>) -> String {
        serve(responses, false)
    }

    fn serve(responses: Vec<(u16, Vec<u8>)>, declare_length: bool) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                read_request(&mut stream);
                let length = if declare_length {
                    format!("content-length: {}\r\n", body.len())
                } else {
                    String::new()
                };
                let mut response = format!(
                    "HTTP/1.1 {status} STATUS\r\ncontent-type: application/json\r\n{length}connection: close\r\n\r\n"
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response);
            }
            close_unanswered(listener);
        });
        format!("http://{addr}/toy-secret-path")
    }

    /// Reads one request, head and declared body, or as much of it as the
    /// client sends before it closes or fails.
    fn read_request(stream: &mut std::net::TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let head_end = loop {
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
            match stream.read(&mut buffer) {
                Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                _ => return,
            }
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < head_end + length {
            match stream.read(&mut buffer) {
                Ok(read) if read > 0 => request.extend_from_slice(&buffer[..read]),
                _ => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const SECRET_TARGET: &str = "/toy-secret-path?token=toy-secret-query";

    fn loopback_client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    fn assert_no_url(text: &str) {
        assert!(!text.is_empty());
        for leaked in [
            "toy-secret-path",
            "toy-secret-query",
            "127.0.0.1",
            "for url",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text:?}");
        }
    }

    /// Answers one request with `response`. With `hold_open` the connection
    /// stays open until the client drops it, so a reader that waited for the
    /// rest of the body would never return.
    async fn serve_once(response: Vec<u8>, hold_open: bool) -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).await.unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(&response).await.unwrap();
            if hold_open {
                while stream.read(&mut buffer).await.unwrap_or(0) > 0 {}
            }
        });
        addr
    }

    async fn fetch(addr: SocketAddr, target: &str, limit: usize) -> Result<Vec<u8>, BodyReadError> {
        let response = loopback_client()
            .get(format!("http://{addr}{target}"))
            .send()
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), read_body_capped(response, limit))
            .await
            .expect("the read must end without waiting for the rest of the body")
    }

    #[tokio::test]
    async fn error_text_of_a_send_error_names_no_url() {
        let error = loopback_client()
            .get(test_support::unanswered_secret_url())
            .send()
            .await
            .unwrap_err();
        assert!(error.is_request(), "{error:?}");
        assert!(error.to_string().contains("toy-secret-query"), "{error}");
        assert_no_url(&error_text(error));
    }

    #[test]
    fn a_blocking_send_error_carries_the_url_that_url_free_error_drops() {
        let error = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(test_support::unanswered_secret_url())
            .send()
            .unwrap_err();
        assert!(error.is_request(), "{error:?}");
        assert!(error.to_string().contains("toy-secret-query"), "{error}");
        assert_no_url(&format!("{:#}", url_free_error(error)));
    }

    #[tokio::test]
    async fn a_body_under_the_limit_is_read_whole() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-length: 5\r\nconnection: close\r\n\r\nhello".to_vec(),
            false,
        )
        .await;
        assert_eq!(fetch(addr, "/", 16).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn a_body_of_exactly_the_limit_is_read_whole() {
        let declared =
            b"HTTP/1.1 200 OK\r\ncontent-length: 16\r\nconnection: close\r\n\r\n0123456789abcdef";
        let addr = serve_once(declared.to_vec(), false).await;
        assert_eq!(fetch(addr, "/", 16).await.unwrap(), b"0123456789abcdef");

        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n8\r\n01234567\r\n8\r\n89abcdef\r\n0\r\n\r\n";
        let addr = serve_once(chunked.to_vec(), false).await;
        assert_eq!(fetch(addr, "/", 16).await.unwrap(), b"0123456789abcdef");
    }

    #[tokio::test]
    async fn a_declared_length_over_the_limit_is_refused_before_reading() {
        // No body follows the head: a reader that tried to read it would wait.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n".to_vec(),
            true,
        )
        .await;
        let error = fetch(addr, "/", 16).await.unwrap_err();
        assert_eq!(error, BodyReadError::TooLarge { limit: 16 });
        assert_eq!(error.to_string(), "response body exceeded 16 bytes");
    }

    #[tokio::test]
    async fn an_undeclared_body_over_the_limit_is_refused_while_reading() {
        // The chunked body never ends, so only a reader that stops at the
        // limit returns.
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n8\r\n01234567\r\n9\r\n89abcdefg\r\n"
                .to_vec(),
            true,
        )
        .await;
        assert_eq!(
            fetch(addr, "/", 16).await.unwrap_err(),
            BodyReadError::TooLarge { limit: 16 }
        );
    }

    #[tokio::test]
    async fn a_connection_cut_mid_body_is_a_transport_error_without_url() {
        let addr = serve_once(
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789".to_vec(),
            false,
        )
        .await;
        match fetch(addr, SECRET_TARGET, 1024).await.unwrap_err() {
            BodyReadError::Transport(text) => assert_no_url(&text),
            other => panic!("expected a transport error, got {other:?}"),
        }
    }

    /// The blocking counterpart of [`serve_once`], on a plain thread.
    fn serve_once_blocking(response: Vec<u8>, hold_open: bool) -> SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            stream.write_all(&response).unwrap();
            if hold_open {
                while stream.read(&mut buffer).unwrap_or(0) > 0 {}
            }
        });
        addr
    }

    fn fetch_blocking(
        addr: SocketAddr,
        target: &str,
        limit: usize,
    ) -> Result<Vec<u8>, BodyReadError> {
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
            .get(format!("http://{addr}{target}"))
            .send()
            .unwrap();
        read_blocking_body_capped(response, limit, Duration::from_secs(5))
    }

    #[test]
    fn a_blocking_body_that_trickles_in_stops_at_its_deadline() {
        // 80 bytes, one every 100 ms. The client's own timeout is far longer
        // than the trickle, and reqwest's `Read` starts it again for every
        // read, so only the deadline over the whole body ends the read before
        // the trickle does, 8 s in.
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|part| part == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before its headers");
                request.extend_from_slice(&buffer[..read]);
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 80\r\nconnection: close\r\n\r\n");
            for _ in 0..80 {
                if stream.write_all(b"a").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let deadline = Duration::from_secs(1);
        let response = reqwest::blocking::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap()
            .get(format!("http://{addr}/"))
            .send()
            .unwrap();
        let started = Instant::now();
        let result = read_blocking_body_capped(response, 1024, deadline);
        let elapsed = started.elapsed();
        assert_eq!(
            result,
            Err(BodyReadError::Transport(BODY_READ_TIMEOUT_TEXT.to_string()))
        );
        // The deadline is 1 s and a read under way returns with the next
        // byte; the rest is room for a loaded runner, still short of the 8 s
        // trickle.
        assert!(elapsed < Duration::from_secs(6), "took {elapsed:?}");
    }

    #[test]
    fn a_blocking_body_of_exactly_the_limit_is_read_whole() {
        let declared =
            b"HTTP/1.1 200 OK\r\ncontent-length: 16\r\nconnection: close\r\n\r\n0123456789abcdef";
        let addr = serve_once_blocking(declared.to_vec(), false);
        assert_eq!(fetch_blocking(addr, "/", 16).unwrap(), b"0123456789abcdef");

        let chunked = b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n8\r\n01234567\r\n8\r\n89abcdef\r\n0\r\n\r\n";
        let addr = serve_once_blocking(chunked.to_vec(), false);
        assert_eq!(fetch_blocking(addr, "/", 16).unwrap(), b"0123456789abcdef");
    }

    #[test]
    fn a_blocking_declared_length_over_the_limit_is_refused_before_reading() {
        let addr = serve_once_blocking(
            b"HTTP/1.1 200 OK\r\ncontent-length: 17\r\n\r\n".to_vec(),
            true,
        );
        assert_eq!(
            fetch_blocking(addr, "/", 16).unwrap_err(),
            BodyReadError::TooLarge { limit: 16 }
        );
    }

    #[test]
    fn a_blocking_undeclared_body_over_the_limit_is_refused_while_reading() {
        // The chunked body never ends; the 5 s client timeout would turn a
        // reader that waited for the rest into a transport error.
        let addr = serve_once_blocking(
            b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n8\r\n01234567\r\n9\r\n89abcdefg\r\n"
                .to_vec(),
            true,
        );
        assert_eq!(
            fetch_blocking(addr, "/", 16).unwrap_err(),
            BodyReadError::TooLarge { limit: 16 }
        );
    }

    #[test]
    fn a_blocking_connection_cut_mid_body_is_a_transport_error_without_url() {
        let addr = serve_once_blocking(
            b"HTTP/1.1 200 OK\r\ncontent-length: 100\r\n\r\n0123456789".to_vec(),
            false,
        );
        match fetch_blocking(addr, SECRET_TARGET, 1024).unwrap_err() {
            BodyReadError::Transport(text) => assert_no_url(&text),
            other => panic!("expected a transport error, got {other:?}"),
        }
    }

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(
            escape_control_chars("a\u{1b}[2Jb\u{7}c\rd\ne\u{0}f\u{9b}g"),
            "a\\u{1b}[2Jb\\u{7}c\\rd\\ne\\u{0}f\\u{9b}g"
        );
    }

    #[test]
    fn printable_text_is_kept_as_is() {
        let text = "Visit https://example.test/device?code=AB-12 — код «ok» \\n";
        assert_eq!(escape_control_chars(text), text);
    }
}
