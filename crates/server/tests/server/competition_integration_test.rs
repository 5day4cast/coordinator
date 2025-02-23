use anyhow::Result;
use dlctix::secp::Point;

use crate::helpers::{create_test_nostr_client, create_test_wallet};

#[tokio::test]
async fn test_dlc_public_key_format() -> Result<()> {
    let nostr_client = create_test_nostr_client().await;
    let wallet = create_test_wallet(&nostr_client).await;

    let pubkey = wallet.get_dlc_public_key(0).await.unwrap();

    // Verify format
    assert!(pubkey.starts_with("02"), "Public key should start with 02");
    assert_eq!(
        pubkey.len(),
        66,
        "Public key should be 33 bytes (66 hex chars including prefix)"
    );

    // Verify it can be parsed by Point
    Point::from_hex(&pubkey).expect("Should parse as Point");

    Ok(())
}
