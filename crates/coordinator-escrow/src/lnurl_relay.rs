//! Bounded control protocol for the untrusted LNURL TCP relay.
//!
//! DNS answers are hints. The enclave validates every address and supplies the
//! exact IP for connection; TLS certificate and hostname checks stay in the enclave.

use anyhow::{ensure, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_CONTROL_BYTES: usize = 4096;
pub const MAX_DNS_ADDRESSES: usize = 16;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RelayRequest {
    Resolve {
        host: String,
        port: u16,
    },
    /// An IP literal, never a hostname that can be resolved a second time.
    Connect {
        address: SocketAddr,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub enum RelayResponse {
    Addresses(Vec<SocketAddr>),
    Connected,
    /// Deliberately no upstream error text or URLs containing provider tokens.
    Rejected,
}

pub async fn read_control<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> Result<T> {
    let len = reader.read_u32().await? as usize;
    ensure!(
        len > 0 && len <= MAX_CONTROL_BYTES,
        "Invalid LNURL relay frame length"
    );
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes).await?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub async fn write_control<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> Result<()> {
    let bytes = serde_json::to_vec(value)?;
    ensure!(
        bytes.len() <= MAX_CONTROL_BYTES,
        "LNURL relay frame too large"
    );
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

pub fn validate_dns_host(host: &str) -> Result<()> {
    ensure!(
        !host.is_empty()
            && host.len() <= 253
            && host.contains('.')
            && !host.to_ascii_lowercase().ends_with(".onion")
            && host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-')
            }),
        "Invalid LNURL DNS hostname"
    );
    Ok(())
}

pub fn validate_addresses(addresses: &[SocketAddr], port: u16) -> Result<()> {
    ensure!(
        port != 0 && !addresses.is_empty() && addresses.len() <= MAX_DNS_ADDRESSES,
        "Invalid LNURL DNS answer count or port"
    );
    // Reject an entire mixed answer, including when a public address comes first.
    ensure!(
        addresses
            .iter()
            .all(|a| a.port() == port && is_public_ip(a.ip())),
        "LNURL destination is not a public address on the requested port"
    );
    Ok(())
}

pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || (a == 100 && (64..=127).contains(&b))
                || a == 127
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192 && b == 0 && (c == 0 || c == 2))
                || (a == 192 && b == 88 && c == 99)
                || (a == 192 && b == 168)
                || (a == 198 && (b == 18 || b == 19))
                || (a == 198 && b == 51 && c == 100)
                || (a == 203 && b == 0 && c == 113)
                || a >= 224)
        }
        IpAddr::V6(ip) => {
            let [a, b, ..] = ip.segments();
            // Ordinary global unicast only: no local, mapped, NAT64 or multicast.
            (a & 0xe000) == 0x2000
                && !(a == 0x2001 && b < 0x0200)
                && !(a == 0x2001 && b == 0x0db8)
                && a != 0x2002 // 6to4 can embed a private IPv4 destination.
                && a != 0x3ffe // Retired 6bone.
                && !(a == 0x3fff && b < 0x1000) // Documentation (RFC 9637).
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_special_address_ranges_and_mixed_dns_answers() {
        for ip in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "100.127.255.255",
            "127.0.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.9",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.19.255.255",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:8.8.8.8",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "64:ff9b::7f00:1",
            "64:ff9b:1::1",
            "100::1",
            "2001::7f00:1",
            "2001:2::1",
            "2001:20::1",
            "2001:db8::1",
            "2002:7f00:1::1",
            "3ffe:831f::1",
            "3fff::1",
            "3fff:fff::1",
            "5f00::1",
            "fc00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
        ] {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip}");
        }
        let public = "8.8.8.8:443".parse().unwrap();
        let private = "127.0.0.1:443".parse().unwrap();
        assert!(validate_addresses(&[], 443).is_err());
        assert!(validate_addresses(&[public, private], 443).is_err());
        assert!(validate_addresses(&[private, public], 443).is_err());
        assert!(validate_addresses(&[public], 8443).is_err());
        assert!(validate_addresses(&[public; MAX_DNS_ADDRESSES + 1], 443).is_err());
    }

    #[test]
    fn accepts_public_range_boundaries() {
        for ip in [
            "1.1.1.1",
            "8.8.8.8",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "2001:200::1",
            "2001:4860:4860::8888",
            "2606:4700:4700::1111",
            "2a00:1450::1",
        ] {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn oversized_frames_are_rejected_before_allocating_the_body() {
        let bytes = ((MAX_CONTROL_BYTES + 1) as u32).to_be_bytes();
        assert!(read_control::<_, RelayRequest>(&mut &bytes[..])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn connect_protocol_accepts_only_an_ip_literal() {
        let input = br#"{"Connect":{"address":"attacker.example:443"}}"#;
        let mut bytes = (input.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(input);
        assert!(read_control::<_, RelayRequest>(&mut &bytes[..])
            .await
            .is_err());
    }
}
