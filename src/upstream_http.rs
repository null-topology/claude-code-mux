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
}
