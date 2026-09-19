//! Browser-side client for the coordinator: the user's Nostr identity (NIP-98
//! auth, password login) and the DLC entry wallet. See `wallet` for the trust
//! boundary.

pub mod nostr;
pub mod wallet;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(start)]
pub fn start() {
    wasm_logger::init(wasm_logger::Config::default());
}
