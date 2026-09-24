pub mod admin;
pub mod assets;
pub mod components;
pub mod format;
pub mod fragments;
pub mod layouts;
pub mod pages;
pub mod qr;
pub mod shared_map;

/// A short retry catches a cache fill just beyond the initial page budget. Later attempts
/// back off so a slow or unavailable oracle still ends in a bounded, manual retry.
pub fn loading_retry(attempt: u8) -> Option<&'static str> {
    [
        "load delay:50ms",
        "load delay:250ms",
        "load delay:1s",
        "load delay:3s",
    ]
    .get(usize::from(attempt))
    .copied()
}
