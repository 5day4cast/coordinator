//! Export the real signup templates and assets for the offline browser test.
use coordinator::templates::{
    assets,
    components::{mainnet_signup::*, RecoveryHelp},
    layouts::base::{base, PageConfig},
};
use std::{error::Error, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn Error>> {
    let output = PathBuf::from(std::env::args().nth(1).ok_or("expected output directory")?);
    fs::create_dir_all(output.join("assets"))?;
    for asset in assets::ALL {
        fs::write(output.join(asset.url.trim_start_matches('/')), asset.bytes)?;
    }
    let config = PageConfig {
        title: "Mainnet signup",
        api_base: "",
        oracle_base: "",
        network: "signet",
        wasm_version: "fixture",
        recovery: RecoveryHelp::FileAndRelays,
        satchel_url: None,
        feedback: true,
        mainnet_signup: true,
    };
    fs::write(
        output.join("index.html"),
        base(&config, mainnet_signup_page(mainnet_signup_form("", None))).into_string(),
    )?;
    fs::write(
        output.join("thanks.html"),
        mainnet_signup_thanks(false).into_string(),
    )?;
    fs::write(
        output.join("retry.html"),
        mainnet_signup_form(
            "player@example.com",
            Some("This form went stale. Press Notify me again."),
        )
        .into_string(),
    )?;
    Ok(())
}
