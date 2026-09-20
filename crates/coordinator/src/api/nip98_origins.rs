//! The origins a NIP-98 event may name.
//!
//! The event's `u` tag must be the exact request URL. Rebuilding that URL
//! from the `Host` header would let a client choose the host, so the origin
//! comes from configuration and only the path and query come from the
//! request.

use anyhow::{anyhow, bail};
use axum::http::Uri;
use nostr_sdk::Url;
use std::collections::BTreeSet;

pub struct Nip98Origins {
    /// `scheme://host[:port]`, default ports omitted.
    origins: BTreeSet<String>,
}

impl Nip98Origins {
    pub fn new<I, S>(origins: I) -> Result<Self, anyhow::Error>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut set = BTreeSet::new();
        for origin in origins {
            let origin = origin.as_ref().trim();
            let url = Url::parse(origin).map_err(|e| anyhow!("invalid origin {origin:?}: {e}"))?;
            if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
                bail!("origin {origin:?} must be an http(s) URL with a host");
            }
            set.insert(url.origin().ascii_serialization());
        }
        if set.is_empty() {
            bail!("at least one NIP-98 origin is required");
        }
        Ok(Self { origins: set })
    }

    /// Whether `url` is `uri`'s path and query on one of the allowed origins.
    pub fn accepts(&self, url: &Url, uri: &Uri) -> bool {
        let requested = uri.path_and_query().map(|p| p.as_str()).unwrap_or("/");
        let named = match url.query() {
            Some(query) => format!("{}?{}", url.path(), query),
            None => url.path().to_string(),
        };
        self.origins.contains(&url.origin().ascii_serialization()) && named == requested
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn origins() -> Nip98Origins {
        Nip98Origins::new(["http://localhost:9990", "https://Weather.Example/"]).unwrap()
    }

    fn accepts(url: &str, uri: &str) -> bool {
        origins().accepts(&Url::parse(url).unwrap(), &Uri::from_str(uri).unwrap())
    }

    #[test]
    fn matches_path_and_query_on_a_configured_origin() {
        assert!(accepts(
            "http://localhost:9990/api/v1/entries?x=1",
            "/api/v1/entries?x=1"
        ));
        assert!(accepts(
            "https://weather.example/api/v1/users/login",
            "/api/v1/users/login"
        ));
        assert!(accepts("https://weather.example:443/api", "/api"));
    }

    #[test]
    fn rejects_other_hosts_schemes_ports_and_paths() {
        assert!(!accepts(
            "http://evil.example/api/v1/entries",
            "/api/v1/entries"
        ));
        assert!(!accepts(
            "http://localhost:9991/api/v1/entries",
            "/api/v1/entries"
        ));
        assert!(!accepts(
            "https://localhost:9990/api/v1/entries",
            "/api/v1/entries"
        ));
        assert!(!accepts(
            "http://localhost:9990/api/v1/other",
            "/api/v1/entries"
        ));
        assert!(!accepts(
            "http://localhost:9990/api/v1/entries",
            "/api/v1/entries?x=1"
        ));
    }

    #[test]
    fn origins_must_be_http_urls() {
        assert!(Nip98Origins::new(["ftp://x.example"]).is_err());
        assert!(Nip98Origins::new(["not a url"]).is_err());
        assert!(Nip98Origins::new(Vec::<&str>::new()).is_err());
    }
}
