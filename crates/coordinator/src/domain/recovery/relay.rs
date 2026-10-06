//! Publishing events to a Nostr relay: a WebSocket client for NIP-01's `EVENT` and `OK` only.
//!
//! The coordinator only ever sends events and reads the relay's answers, so it does not need
//! a relay pool, subscriptions or a local event database.

use anyhow::{anyhow, bail};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use nostr::Url;
use rustls::{pki_types::ServerName, ClientConfig, RootCertStore};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::TlsConnector;

/// Largest relay message read. Answers to `EVENT` are short.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
const MAX_HANDSHAKE_BYTES: usize = 16 * 1024;

const OPCODE_CONTINUATION: u8 = 0x0;
const OPCODE_TEXT: u8 = 0x1;
const OPCODE_BINARY: u8 = 0x2;
const OPCODE_CLOSE: u8 = 0x8;
const OPCODE_PING: u8 = 0x9;
const OPCODE_PONG: u8 = 0xA;

static TLS: LazyLock<Arc<ClientConfig>> = LazyLock::new(|| {
    let roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("ring supports the default protocol versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
    Arc::new(config)
});

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

/// The relay's answer to each event, by event id: accepted, or its reason for refusing.
pub type RelayAnswers = HashMap<String, Result<(), String>>;

/// Send `events` (`(id, event JSON)` pairs) to the relay at `url` and wait for its `OK` to each,
/// for at most `wait` in all.
pub async fn publish(
    url: &str,
    events: &[(String, String)],
    wait: Duration,
) -> Result<RelayAnswers, anyhow::Error> {
    tokio::time::timeout(wait, async {
        let mut connection = Connection::open(url).await?;
        for (_, event) in events {
            connection
                .send(OPCODE_TEXT, format!(r#"["EVENT",{event}]"#).as_bytes())
                .await?;
        }
        let mut answers = RelayAnswers::new();
        while answers.len() < events.len() {
            let message = connection.next_text().await?;
            if let Some((id, answer)) = parse_ok(&message) {
                if events.iter().any(|(event_id, _)| *event_id == id) {
                    answers.insert(id, answer);
                }
            }
        }
        connection.close().await;
        Ok(answers)
    })
    .await
    .map_err(|_| anyhow!("no answer within {}s", wait.as_secs()))?
}

/// `["OK", <id>, <accepted>, <message>]`.
fn parse_ok(message: &str) -> Option<(String, Result<(), String>)> {
    let value: serde_json::Value = serde_json::from_str(message).ok()?;
    let fields = value.as_array()?;
    if fields.first()?.as_str()? != "OK" {
        return None;
    }
    let id = fields.get(1)?.as_str()?.to_owned();
    let accepted = fields.get(2)?.as_bool()?;
    let reason = fields
        .get(3)
        .and_then(|reason| reason.as_str())
        .unwrap_or_default();
    Some((
        id,
        if accepted {
            Ok(())
        } else {
            Err(reason.to_owned())
        },
    ))
}

struct Connection {
    stream: Box<dyn Stream>,
    /// Bytes read and not yet parsed.
    buffer: Vec<u8>,
}

impl Connection {
    async fn open(url: &str) -> Result<Self, anyhow::Error> {
        let url = Url::parse(url)?;
        let secure = match url.scheme() {
            "wss" => true,
            "ws" => false,
            scheme => bail!("relays are ws:// or wss://, not {scheme}://"),
        };
        let host = url
            .host_str()
            .ok_or_else(|| anyhow!("relay URL has no host"))?
            .to_owned();
        let default_port = if secure { 443 } else { 80 };
        let port = url.port().unwrap_or(default_port);
        let tcp = TcpStream::connect((host.as_str(), port)).await?;
        tcp.set_nodelay(true)?;
        let stream: Box<dyn Stream> = if secure {
            let name = ServerName::try_from(host.clone())
                .map_err(|_| anyhow!("relay host {host} is not a valid TLS name"))?;
            Box::new(TlsConnector::from(TLS.clone()).connect(name, tcp).await?)
        } else {
            Box::new(tcp)
        };
        let mut connection = Self {
            stream,
            buffer: Vec::new(),
        };
        let mut target = url.path().to_owned();
        if let Some(query) = url.query() {
            target.push('?');
            target.push_str(query);
        }
        let host_header = if port == default_port {
            host
        } else {
            format!("{host}:{port}")
        };
        connection.handshake(&target, &host_header).await?;
        Ok(connection)
    }

    async fn handshake(&mut self, target: &str, host: &str) -> Result<(), anyhow::Error> {
        let key = BASE64.encode(rand::random::<[u8; 16]>());
        let request = format!(
            "GET {target} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        self.stream.write_all(request.as_bytes()).await?;
        self.stream.flush().await?;
        let end = loop {
            if let Some(end) = self
                .buffer
                .windows(4)
                .position(|bytes| bytes == b"\r\n\r\n")
            {
                break end;
            }
            if self.buffer.len() > MAX_HANDSHAKE_BYTES {
                bail!("relay sent no WebSocket handshake");
            }
            self.fill().await?;
        };
        let head = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
        self.buffer.drain(..end + 4);
        let status = head.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            bail!("relay refused the WebSocket upgrade: {status}");
        }
        Ok(())
    }

    async fn fill(&mut self) -> Result<(), anyhow::Error> {
        let mut chunk = [0u8; 8192];
        let read = self.stream.read(&mut chunk).await?;
        if read == 0 {
            bail!("relay closed the connection");
        }
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(())
    }

    async fn fill_to(&mut self, len: usize) -> Result<(), anyhow::Error> {
        while self.buffer.len() < len {
            self.fill().await?;
        }
        Ok(())
    }

    /// Send one frame. A client masks every frame it sends.
    async fn send(&mut self, opcode: u8, payload: &[u8]) -> Result<(), anyhow::Error> {
        let mut frame = Vec::with_capacity(payload.len() + 14);
        frame.push(0x80 | opcode);
        match payload.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len if len <= usize::from(u16::MAX) => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        let mask: [u8; 4] = rand::random();
        frame.extend_from_slice(&mask);
        frame.extend(
            payload
                .iter()
                .enumerate()
                .map(|(index, byte)| byte ^ mask[index % 4]),
        );
        self.stream.write_all(&frame).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// One frame: whether it is final, its opcode and its unmasked payload.
    async fn read_frame(&mut self) -> Result<(bool, u8, Vec<u8>), anyhow::Error> {
        self.fill_to(2).await?;
        let fin = self.buffer[0] & 0x80 != 0;
        let opcode = self.buffer[0] & 0x0f;
        let masked = self.buffer[1] & 0x80 != 0;
        let (len, mut offset) = match self.buffer[1] & 0x7f {
            126 => {
                self.fill_to(4).await?;
                (
                    u64::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]])),
                    4,
                )
            }
            127 => {
                self.fill_to(10).await?;
                let bytes: [u8; 8] = self.buffer[2..10].try_into()?;
                (u64::from_be_bytes(bytes), 10)
            }
            len => (u64::from(len), 2),
        };
        if len > MAX_MESSAGE_BYTES as u64 {
            bail!("relay message of {len} bytes is too large");
        }
        let mask = if masked {
            self.fill_to(offset + 4).await?;
            let mask: [u8; 4] = self.buffer[offset..offset + 4].try_into()?;
            offset += 4;
            Some(mask)
        } else {
            None
        };
        let end = offset + len as usize;
        self.fill_to(end).await?;
        let mut payload = self.buffer[offset..end].to_vec();
        if let Some(mask) = mask {
            for (index, byte) in payload.iter_mut().enumerate() {
                *byte ^= mask[index % 4];
            }
        }
        self.buffer.drain(..end);
        Ok((fin, opcode, payload))
    }

    /// The next text message, answering pings on the way and skipping binary messages.
    async fn next_text(&mut self) -> Result<String, anyhow::Error> {
        let mut message = Vec::new();
        let mut in_binary = false;
        loop {
            let (fin, opcode, payload) = self.read_frame().await?;
            match opcode {
                OPCODE_TEXT | OPCODE_CONTINUATION if !in_binary => {
                    message.extend_from_slice(&payload);
                    if message.len() > MAX_MESSAGE_BYTES {
                        bail!("relay message is too large");
                    }
                    if fin {
                        return Ok(String::from_utf8(message)?);
                    }
                }
                OPCODE_BINARY | OPCODE_CONTINUATION => in_binary = !fin,
                OPCODE_CLOSE => bail!("relay closed the connection"),
                OPCODE_PING => self.send(OPCODE_PONG, &payload).await?,
                _ => {}
            }
        }
    }

    async fn close(mut self) {
        let _ = self.send(OPCODE_CLOSE, &1000u16.to_be_bytes()).await;
        let _ = self.stream.shutdown().await;
    }
}

#[cfg(test)]
pub(super) mod test_relay {
    //! A relay on a local port that answers every event it is sent.

    use super::*;
    use tokio::net::TcpListener;

    /// How the test relay answers an event.
    pub type Answer = Arc<dyn Fn(&serde_json::Value) -> Option<(bool, String)> + Send + Sync>;

    /// Serve connections until the test ends; returns the relay's `ws://` URL. `answer` decides
    /// each event's `OK`, or no answer at all.
    pub async fn spawn(answer: Answer) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let answer = answer.clone();
                tokio::spawn(async move {
                    let _ = serve(socket, answer).await;
                });
            }
        });
        url
    }

    async fn serve(mut socket: TcpStream, answer: Answer) -> Result<(), anyhow::Error> {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0u8; 1];
            if socket.read(&mut byte).await? == 0 {
                return Ok(());
            }
            head.push(byte[0]);
        }
        socket
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await?;
        let mut connection = Connection {
            stream: Box::new(socket),
            buffer: Vec::new(),
        };
        loop {
            let (_, opcode, payload) = connection.read_frame().await?;
            if opcode == OPCODE_CLOSE {
                return Ok(());
            }
            let message: serde_json::Value = serde_json::from_slice(&payload)?;
            let event = &message[1];
            if let Some((accepted, reason)) = answer(event) {
                let id = event["id"].as_str().unwrap_or_default();
                let reply = serde_json::json!(["OK", id, accepted, reason]).to_string();
                connection
                    .send_unmasked(OPCODE_TEXT, reply.as_bytes())
                    .await?;
            }
        }
    }

    impl Connection {
        /// A server's frame, which is not masked.
        async fn send_unmasked(&mut self, opcode: u8, payload: &[u8]) -> Result<(), anyhow::Error> {
            let mut frame = vec![0x80 | opcode];
            if payload.len() < 126 {
                frame.push(payload.len() as u8);
            } else {
                frame.push(126);
                frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
            }
            frame.extend_from_slice(payload);
            self.stream.write_all(&frame).await?;
            Ok(())
        }
    }
}
