use std::fmt;

/// The `Display` text of `error` with its URL removed. A reqwest error that
/// holds a URL ends its text with ` for url (<url>)`, and an upstream URL can
/// carry a credential in its path or query, so the text that reaches a client,
/// the CLI or `proxy.log` must never include it. `Debug` and the source chain
/// are left out for the same reason.
// Call sites follow in the same pull request.
#[allow(dead_code)]
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
// Call sites follow in the same pull request.
#[allow(dead_code)]
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

/// [`read_body_capped`] for a response of the blocking client: a declared
/// length over `limit` is refused before anything is read, and the read stops
/// one byte past `limit`, so no more than that is ever held.
pub(crate) fn read_blocking_body_capped(
    response: reqwest::blocking::Response,
    limit: usize,
) -> Result<Vec<u8>, BodyReadError> {
    use std::io::Read;

    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(BodyReadError::TooLarge { limit });
    }
    let mut body = Vec::new();
    response
        .take((limit as u64).saturating_add(1))
        .read_to_end(&mut body)
        .map_err(blocking_read_error)?;
    if body.len() > limit {
        return Err(BodyReadError::TooLarge { limit });
    }
    Ok(body)
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

/// A JSON answer of the blocking client, read under `limit` with
/// [`read_blocking_body_capped`] and parsed with [`decode_json`].
pub(crate) fn read_blocking_json_capped<T: serde::de::DeserializeOwned>(
    response: reqwest::blocking::Response,
    limit: usize,
) -> anyhow::Result<T> {
    decode_json(&read_blocking_body_capped(response, limit)?)
}

/// A body of the blocking client as text for an error message, empty when it
/// cannot be read or is over `limit`, as reqwest's `text().unwrap_or_default()`
/// gives for a body it cannot read.
pub(crate) fn blocking_error_text(response: reqwest::blocking::Response, limit: usize) -> String {
    read_blocking_body_capped(response, limit)
        .map(|body| String::from_utf8_lossy(&body).into_owned())
        .unwrap_or_default()
}

/// Loopback servers for the provider tests.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};

    /// A URL on loopback that refuses connections, with a toy secret in its
    /// path and query.
    pub(crate) fn refused_secret_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        format!("http://127.0.0.1:{port}/toy-secret-path?token=toy-secret-query")
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
    /// its status and body and `connection: close`, then stops listening, so
    /// a later request is refused. Returns `http://<addr>/toy-secret-path`.
    pub(crate) fn serve_in_order(responses: Vec<(u16, Vec<u8>)>) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().unwrap();
                read_request(&mut stream);
                let mut response = format!(
                    "HTTP/1.1 {status} STATUS\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                response.extend_from_slice(&body);
                let _ = stream.write_all(&response);
            }
        });
        format!("http://{addr}/toy-secret-path")
    }

    fn read_request(stream: &mut std::net::TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let head_end = loop {
            if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                break end + 4;
            }
            let read = stream.read(&mut buffer).unwrap();
            assert!(read > 0, "request ended before its headers");
            request.extend_from_slice(&buffer[..read]);
        };
        let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while request.len() < head_end + length {
            let read = stream.read(&mut buffer).unwrap();
            if read == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..read]);
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
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = loopback_client()
            .get(format!("http://127.0.0.1:{port}{SECRET_TARGET}"))
            .send()
            .await
            .unwrap_err();
        assert!(error.url().is_some());
        assert_no_url(&error_text(error));
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
        read_blocking_body_capped(response, limit)
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
}
