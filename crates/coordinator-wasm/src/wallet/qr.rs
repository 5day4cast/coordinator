//! QR codes for Lightning invoices, drawn in the wallet from the invoice it
//! has just checked, so the page needs no QR script and cannot show a code
//! for a different invoice than the one validated.

use base64::{engine::general_purpose::STANDARD, Engine};
use qrcodegen::{QrCode, QrCodeEcc};

use super::WalletError;

/// Modules of blank border around the code, as the QR standard asks.
const QUIET_ZONE: i32 = 4;

/// A QR code for a Lightning invoice, as an SVG `data:` URL for an `<img>`,
/// where it can run nothing.
///
/// Upper case lets the invoice use the QR code's compact alphanumeric mode,
/// which gives a smaller, easier-to-scan code; wallets read the scheme and
/// the invoice either way.
pub fn lightning_invoice_data_url(invoice: &str) -> Result<String, WalletError> {
    let text = format!("LIGHTNING:{}", invoice.trim().to_ascii_uppercase());
    let code = QrCode::encode_text(&text, QrCodeEcc::Medium)
        .map_err(|_| WalletError::Invoice("too long for a QR code".into()))?;
    Ok(format!(
        "data:image/svg+xml;base64,{}",
        STANDARD.encode(svg(&code))
    ))
}

fn svg(code: &QrCode) -> String {
    let size = code.size() + 2 * QUIET_ZONE;
    let mut path = String::new();
    for y in 0..code.size() {
        for x in 0..code.size() {
            if code.get_module(x, y) {
                path.push_str(&format!("M{},{}h1v1h-1z", x + QUIET_ZONE, y + QUIET_ZONE));
            }
        }
    }
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {size} {size}" shape-rendering="crispEdges"><rect width="{size}" height="{size}" fill="#fff"/><path d="{path}" fill="#000"/></svg>"##
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invoice_becomes_an_svg_qr_code() {
        let url =
            lightning_invoice_data_url("lnbcrt52500n1pnexample0dqqcqzzsxqyz5vqsp5example9qyyssq")
                .unwrap();
        let encoded = url.strip_prefix("data:image/svg+xml;base64,").unwrap();
        let svg = String::from_utf8(STANDARD.decode(encoded).unwrap()).unwrap();
        assert!(svg.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 "#));
        assert!(svg.contains(r##"fill="#000""##));
        assert!(!svg.contains("<script"));
    }
}
