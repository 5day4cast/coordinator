pub mod admin_monitoring;
pub mod admin_signals;
pub mod admin_weather;
pub mod ark_swap;
pub mod bitcoin;
pub mod db;
pub mod escrow;
pub mod feedback_alerts;
pub mod file_utils;
pub mod keymeld;
pub mod lightning;
pub mod lnurl;
pub mod oracle;
pub mod oracle_weather;
pub mod refresh_cache;
pub mod secrets;
pub mod visitor_logs;

// Mock implementations only available with e2e-testing feature or debug builds
#[cfg(any(feature = "e2e-testing", debug_assertions))]
pub mod bitcoin_mock;
#[cfg(any(feature = "e2e-testing", debug_assertions))]
pub mod lightning_mock;
#[cfg(any(feature = "e2e-testing", debug_assertions))]
pub mod lnurl_mock;
#[cfg(any(feature = "e2e-testing", debug_assertions))]
pub mod oracle_mock;
