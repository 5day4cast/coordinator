//! Export real payout templates and bundled assets for offline browser QA.
//! cargo run -p coordinator --example payout_ui_fixtures -- /tmp/payout-ui
//! This uses synthetic rows only; it never opens a database or sends requests.
use coordinator::{
    domain::EligiblePayout,
    templates::{
        assets,
        layouts::base::{base, PageConfig},
        pages::payouts::payouts_page,
    },
};
use std::{error::Error, fs, path::PathBuf};
use uuid::Uuid;

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("expected output directory")?);
    fs::create_dir_all(output.join("assets"))?;
    fs::write(
        output.join("csp.txt"),
        coordinator::api::public_headers::content_security_policy(&[], &[]),
    )?;
    for asset in assets::ALL {
        fs::write(output.join(asset.url.trim_start_matches('/')), asset.bytes)?;
    }
    let address = "qa-payout-with-a-long-address@lightning-wallet.example";
    let statuses = [
        "Awaiting invoice",
        "Queued automatically",
        "Retrying automatically",
        "Paid",
        "On-chain settlement",
    ];
    let mut payouts = Vec::new();
    for (index, status) in statuses.into_iter().enumerate() {
        payouts.push(EligiblePayout {
            competition_id: Uuid::from_u128(100 + index as u128),
            entry_id: Uuid::from_u128(200 + index as u128),
            status: status.into(),
            amount_sats: 1_000,
            automatic_lightning_address: Some(address.into()),
            allow_invoice_fallback: index < 3,
            escrow_enabled: true,
        });
    }
    payouts.push(EligiblePayout {
        competition_id: Uuid::from_u128(106),
        entry_id: Uuid::from_u128(206),
        status: "Awaiting invoice".into(),
        amount_sats: 1_000,
        automatic_lightning_address: None,
        allow_invoice_fallback: true,
        escrow_enabled: false,
    });
    payouts.push(EligiblePayout {
        competition_id: Uuid::from_u128(107),
        entry_id: Uuid::from_u128(207),
        status: "On-chain settlement".into(),
        amount_sats: 1_000,
        automatic_lightning_address: None,
        allow_invoice_fallback: false,
        escrow_enabled: false,
    });
    let config = PageConfig {
        title: "Offline payout QA",
        api_base: "",
        oracle_base: "",
        network: "signet",
        wasm_version: "offline-fixture",
        recovery: coordinator::templates::components::RecoveryHelp::FileAndRelays,
        satchel_url: None,
    };
    for (name, rows, default_address) in [
        ("payouts", payouts.as_slice(), Some(address)),
        ("empty", &[][..], None),
    ] {
        fs::write(
            output.join(format!("{name}.html")),
            base(&config, payouts_page(rows, default_address, true, None)).into_string(),
        )?;
    }
    Ok(())
}
