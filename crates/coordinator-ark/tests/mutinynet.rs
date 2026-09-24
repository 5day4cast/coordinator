//! Against Arkade's public Mutinynet server. Needs the network, so it is ignored by default:
//!
//!     cargo test -p coordinator-ark --test mutinynet -- --ignored
//!
//! Set `ARK_SERVER_URL` to try another server.

use std::time::{SystemTime, UNIX_EPOCH};

use bitcoin::key::{Keypair, Secp256k1};
use bitcoin::secp256k1::SecretKey;
use coordinator_ark::ArkServer;
use coordinator_ark_escrow::{EscrowPath, RelativeTimelock};

const MUTINYNET: &str = "https://mutinynet.arkade.sh";

fn xonly(byte: u8) -> bitcoin::XOnlyPublicKey {
    let secret = SecretKey::from_slice(&[byte; 32]).unwrap();
    Keypair::from_secret_key(&Secp256k1::new(), &secret)
        .x_only_public_key()
        .0
}

#[tokio::test]
#[ignore = "needs the Mutinynet Arkade server"]
async fn mutinynet_accepts_entry_escrows() {
    let url = std::env::var("ARK_SERVER_URL").unwrap_or_else(|_| MUTINYNET.into());
    let server = ArkServer::connect(url).await.unwrap();
    let info = server.info();
    let rules = server.rules();
    println!(
        "signer {} exit delay {:?} session {}s dust {}",
        rules.signer, rules.min_exit_delay, info.session_duration, info.dust
    );
    assert!(matches!(rules.min_exit_delay, RelativeTimelock::Seconds(_)));
    assert!(!rules.block_timelocks_allowed);
    assert_eq!(server.hrp(), "tark");

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let week = 7 * 24 * 60 * 60;
    let terms = server
        .escrow_terms(xonly(1), xonly(2), now + week, now)
        .unwrap();
    let escrow = server.entry_escrow(terms).unwrap();
    assert!(escrow
        .tapscript(EscrowPath::Funding)
        .pubkeys()
        .contains(&rules.signer));
    println!(
        "escrow address {}",
        escrow.address(server.hrp()).unwrap().encode()
    );

    // A fresh escrow has never been paid.
    let vtxos = server.escrow_vtxos(&[escrow]).await.unwrap();
    assert!(vtxos.is_empty(), "{vtxos:?}");
}

/// A refund spends the escrow offchain, which passes through a checkpoint output made of the
/// leaf being spent and the server's own exit script. Whoever authorizes a refund recomputes
/// that output, so the server must publish the script, and a player consents to it on entry.
#[tokio::test]
#[ignore = "needs the Mutinynet Arkade server"]
async fn mutinynet_publishes_its_checkpoint_exit_script() {
    let url = std::env::var("ARK_SERVER_URL").unwrap_or_else(|_| MUTINYNET.into());
    let server = ArkServer::connect(url).await.unwrap();
    let exit_script = &server.info().checkpoint_tapscript;
    println!("checkpoint exit script {}", exit_script.to_hex_string());
    assert!(
        !exit_script.is_empty(),
        "the server publishes no checkpoint exit script"
    );

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as u32;
    let terms = server
        .escrow_terms(xonly(1), xonly(2), now + 7 * 24 * 60 * 60, now)
        .unwrap();
    let escrow = server.entry_escrow(terms).unwrap();
    let checkpoint = coordinator_ark_escrow::checkpoint_script_pubkey(
        escrow.script(EscrowPath::Refund),
        exit_script,
    )
    .unwrap();
    println!("refund checkpoint output {}", checkpoint.to_hex_string());
    assert!(checkpoint.is_p2tr());
}
