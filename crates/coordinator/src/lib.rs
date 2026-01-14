pub mod api;
pub mod config;
pub mod domain;
pub mod infra;
pub mod startup;
pub mod templates;

// Re-exports for backward compatibility during migration
pub use api::routes::*;
pub use config::*;
pub use domain::{
    AddEntry, CompetitionStore, Coordinator, Error as CoordinatorError, SearchBy, TicketResponse,
    UserEntry, UserStore,
};
pub use infra::bitcoin::*;
pub use infra::db::*;
pub use infra::escrow::{generate_escrow_tx, get_escrow_outpoint};
pub use infra::file_utils::*;
pub use infra::lightning::*;
pub use infra::oracle::{
    AddEventEntries, AddEventEntry, Error as OracleError, Event as OracleEvent, Oracle,
    OracleClient, ValueOptions, WeatherChoices,
};
pub use infra::secrets::{get_key, SecretKeyHandler};
pub use startup::*;
