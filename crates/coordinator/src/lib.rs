pub mod admin_cli;
pub mod api;
pub mod config;
pub mod domain;
pub mod infra;
pub mod startup;
pub mod templates;

// Entry points for the `coordinator` and `wallet-cli` binaries; everything
// else is reached through its module.
pub use config::{
    get_settings, get_settings_with_cli, setup_logger, BitcoinSettings, Cli, CliSettings, Command,
    ConfigurableSettings, LnSettings,
};
pub use infra::bitcoin::{Bitcoin, BitcoinClient, SendOptions};
pub use startup::Application;
