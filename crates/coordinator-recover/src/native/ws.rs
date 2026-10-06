//! A minimal WebSocket client (RFC 6455), enough to read from Nostr relays: one connection, text
//! messages, ping and close. TLS through rustls with the webpki roots.

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use reqwest::Url;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;

/// The largest message accepted from a relay.
const MAX_MESSAGE: usize = 8 * 1024 * 1024;
const MAX_HEADERS: usize = 16 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

pub struct WebSocket {
    stream: Box<dyn Stream>,
    /// Bytes read past the handshake or the last frame.
    buffer: Vec<u8>,
}

impl WebSocket {
    pub async fn connect(url: &str) -> Result<Self, String> {
        let url = Url::parse(url).map_err(|e| format!("invalid relay URL {url}: {e}"))?;
        let host = url
            .host_str()
            .ok_or_else(|| format!("relay URL {url} has no host"))?
            .to_owned();
        let secure = match url.scheme() {
            "wss" => true,
            "ws" => false,
            other => return Err(format!("relay URL scheme {other} is not ws or wss")),
        };
        let port = url.port().unwrap_or(if secure { 443 } else { 80 });
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect((host.as_str(), port)))
            .await
            .map_err(|_| format!("{host}: connection timed out"))?
            .map_err(|e| format!("{host}: {e}"))?;
        let stream: Box<dyn Stream> = if secure {
            let mut roots = rustls::RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            let config = rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_no_client_auth();
            let name = ServerName::try_from(host.clone()).map_err(|e| format!("{host}: {e}"))?;
            let tls = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
                .map_err(|e| format!("{host}: TLS: {e}"))?;
            Box::new(tls)
        } else {
            Box::new(tcp)
        };

        let mut socket = Self {
            stream,
            buffer: Vec::new(),
        };
        let key = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 16]>());
        let path = match url.query() {
            Some(query) => format!("{}?{query}", url.path()),
            None => url.path().to_owned(),
        };
        let host_header = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {host_header}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        socket
            .stream
            .write_all(request.as_bytes())
            .await
            .map_err(|e| format!("{host}: {e}"))?;
        let headers = socket.read_headers().await?;
        let status = headers.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(format!("{host} refused the WebSocket upgrade: {status}"));
        }
        Ok(socket)
    }

    async fn read_headers(&mut self) -> Result<String, String> {
        loop {
            if let Some(end) = self.buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&self.buffer[..end]).into_owned();
                self.buffer.drain(..end + 4);
                return Ok(headers);
            }
            if self.buffer.len() > MAX_HEADERS {
                return Err("the relay's handshake response is too long".into());
            }
            self.fill().await?;
        }
    }

    async fn fill(&mut self) -> Result<(), String> {
        let mut chunk = [0u8; 8192];
        let read = self
            .stream
            .read(&mut chunk)
            .await
            .map_err(|e| e.to_string())?;
        if read == 0 {
            return Err("the relay closed the connection".into());
        }
        self.buffer.extend_from_slice(&chunk[..read]);
        Ok(())
    }

    async fn take(&mut self, count: usize) -> Result<Vec<u8>, String> {
        while self.buffer.len() < count {
            self.fill().await?;
        }
        Ok(self.buffer.drain(..count).collect())
    }

    pub async fn send_text(&mut self, text: &str) -> Result<(), String> {
        self.send_frame(0x1, text.as_bytes()).await
    }

    /// A client frame: final, masked.
    async fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<(), String> {
        let mut frame = vec![0x80 | opcode];
        match payload.len() {
            len @ 0..=125 => frame.push(0x80 | len as u8),
            len @ 126..=0xffff => {
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
                .map(|(i, byte)| byte ^ mask[i % 4]),
        );
        self.stream
            .write_all(&frame)
            .await
            .map_err(|e| e.to_string())
    }

    /// The next text message, or `None` once the relay closes the connection.
    pub async fn next_text(&mut self) -> Result<Option<String>, String> {
        let mut message: Vec<u8> = Vec::new();
        loop {
            let header = self.take(2).await?;
            let fin = header[0] & 0x80 != 0;
            let opcode = header[0] & 0x0f;
            let masked = header[1] & 0x80 != 0;
            let len = match header[1] & 0x7f {
                126 => {
                    u16::from_be_bytes(self.take(2).await?.try_into().expect("two bytes")) as u64
                }
                127 => u64::from_be_bytes(self.take(8).await?.try_into().expect("eight bytes")),
                len => u64::from(len),
            };
            if len as usize > MAX_MESSAGE || message.len() + len as usize > MAX_MESSAGE {
                return Err("the relay sent a message that is too large".into());
            }
            let mask = if masked {
                Some(self.take(4).await?)
            } else {
                None
            };
            let mut payload = self.take(len as usize).await?;
            if let Some(mask) = mask {
                payload
                    .iter_mut()
                    .enumerate()
                    .for_each(|(i, byte)| *byte ^= mask[i % 4]);
            }
            match opcode {
                0x0 | 0x1 => {
                    message.extend_from_slice(&payload);
                    if fin {
                        return String::from_utf8(message)
                            .map(Some)
                            .map_err(|_| "the relay sent text that is not UTF-8".into());
                    }
                }
                0x8 => return Ok(None),
                0x9 => self.send_frame(0xA, &payload).await?,
                // Pongs, and binary messages Nostr does not use.
                _ => {}
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.send_frame(0x8, &[]).await;
        let _ = self.stream.shutdown().await;
    }
}
