//! A small loopback HTTP server for tests: it records every request it gets
//! and answers each one from a closure. One thread per connection, and every
//! answer closes its connection, so a client never reuses one.

#![allow(dead_code)]

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The Claude model catalog the tests serve instead of the public one.
pub const CATALOG_FIXTURE: &str = include_str!("../fixtures/claude-model-catalog.json");

/// The path the catalog mock is asked on.
pub const CATALOG_PATH: &str = "/model-catalog/v1/catalog.json";

#[derive(Clone, Debug)]
pub struct SeenRequest {
    pub method: String,
    pub target: String,
    pub headers: Vec<(String, String)>,
}

impl SeenRequest {
    /// Every value of a header, matched without regard to case.
    pub fn header_values(&self, name: &str) -> Vec<&str> {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
            .collect()
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.header_values(name).first().copied()
    }

    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or_default()
    }

    /// The query string exactly as it arrived.
    pub fn raw_query(&self) -> &str {
        self.target
            .split_once('?')
            .map(|(_, query)| query)
            .unwrap_or("")
    }

    /// The query pairs, percent-decoded.
    pub fn query_pairs(&self) -> Vec<(String, String)> {
        url::form_urlencoded::parse(self.raw_query().as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    pub fn query(&self, key: &str) -> Option<String> {
        self.query_pairs()
            .into_iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }
}

/// How a reply's body is delimited on the wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Framing {
    /// A `content-length` header.
    Length,
    /// `transfer-encoding: chunked`, in chunks of 64 KiB.
    Chunked,
    /// No length at all: the body ends when the connection closes.
    Close,
}

pub struct MockReply {
    pub status: u16,
    pub body: String,
    pub delay: Duration,
    pub framing: Framing,
    pub headers: Vec<(String, String)>,
}

impl MockReply {
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
            delay: Duration::ZERO,
            framing: Framing::Length,
            headers: Vec::new(),
        }
    }

    /// A 302 to `location`, with an empty body.
    pub fn redirect(location: impl Into<String>) -> Self {
        Self::json(302, "").with_header("location", location)
    }

    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    pub fn framed(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
}

type Reply = dyn Fn(&SeenRequest) -> MockReply + Send + Sync;

pub struct MockHttp {
    pub base: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl MockHttp {
    pub fn start(reply: impl Fn(&SeenRequest) -> MockReply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<SeenRequest>>> = Default::default();
        let reply: Arc<Reply> = Arc::new(reply);
        let recorded = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let seen = Arc::clone(&recorded);
                let reply = Arc::clone(&reply);
                std::thread::spawn(move || serve(stream, &seen, reply.as_ref()));
            }
        });
        Self { base, seen }
    }

    /// A catalog server that answers every request with the fixture.
    pub fn catalog() -> Self {
        Self::start(|_| MockReply::json(200, CATALOG_FIXTURE))
    }

    /// The catalog URL on this server.
    pub fn catalog_url(&self) -> String {
        format!("{}{CATALOG_PATH}", self.base)
    }

    pub fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

fn serve(stream: TcpStream, seen: &Mutex<Vec<SeenRequest>>, reply: &Reply) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let mut headers = Vec::new();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 {
            break;
        }
        let header = header.trim_end_matches(['\r', '\n']);
        if header.is_empty() {
            break;
        }
        if let Some((key, value)) = header.split_once(':') {
            headers.push((key.trim().to_string(), value.trim().to_string()));
        }
    }
    let request = SeenRequest {
        method,
        target,
        headers,
    };
    seen.lock().unwrap().push(request.clone());
    let answer = reply(&request);
    std::thread::sleep(answer.delay);
    let mut stream = stream;
    let mut head = format!(
        "HTTP/1.1 {} Mock\r\ncontent-type: application/json\r\nconnection: close\r\n",
        answer.status
    );
    for (name, value) in &answer.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match answer.framing {
        Framing::Length => head.push_str(&format!("content-length: {}\r\n", answer.body.len())),
        Framing::Chunked => head.push_str("transfer-encoding: chunked\r\n"),
        Framing::Close => {}
    }
    head.push_str("\r\n");
    // A client that stops reading part way closes the connection; the write
    // errors that follow are ignored.
    let _ = stream.write_all(head.as_bytes());
    match answer.framing {
        Framing::Length | Framing::Close => {
            let _ = stream.write_all(answer.body.as_bytes());
        }
        Framing::Chunked => {
            for chunk in answer.body.as_bytes().chunks(64 * 1024) {
                if write!(stream, "{:x}\r\n", chunk.len()).is_err()
                    || stream.write_all(chunk).is_err()
                    || stream.write_all(b"\r\n").is_err()
                {
                    return;
                }
            }
            let _ = stream.write_all(b"0\r\n\r\n");
        }
    }
    let _ = stream.flush();
    // A close-delimited body ends here; the reader holds a second handle on
    // the socket, so close the write side explicitly.
    let _ = stream.shutdown(Shutdown::Write);
}
