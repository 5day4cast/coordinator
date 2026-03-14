pub mod admin;
pub mod auth;
pub mod competitions;
pub mod entries;
pub mod wallet;

use reqwest::Client;

/// HTTP client for the coordinator API
#[derive(Clone)]
pub struct CoordinatorClient {
    http: Client,
    base_url: String,
    admin_url: String,
}

impl CoordinatorClient {
    pub fn new(base_url: &str, admin_url: Option<&str>) -> Self {
        Self {
            http: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            admin_url: admin_url
                .unwrap_or(base_url)
                .trim_end_matches('/')
                .to_string(),
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn admin_url(&self) -> &str {
        &self.admin_url
    }

    pub fn http(&self) -> &Client {
        &self.http
    }
}
