//! QR codes drawn on the server as SVG, so pages need no QR script.

use base64::{engine::general_purpose::STANDARD, Engine};
use qrcodegen::{QrCode, QrCodeEcc};

/// Modules of blank border around the code, as the QR standard asks.
const QUIET_ZONE: i32 = 4;

/// A QR code for a Lightning invoice, as an SVG image.
///
/// Upper case lets the invoice use the QR code's compact alphanumeric mode,
/// which gives a smaller, easier-to-scan code; wallets read the scheme and
/// the invoice either way.
pub fn lightning_invoice_svg(invoice: &str) -> Option<String> {
    let text = format!("LIGHTNING:{}", invoice.trim().to_ascii_uppercase());
    let code = QrCode::encode_text(&text, QrCodeEcc::Medium).ok()?;
    Some(svg(&code))
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

/// An SVG as a `data:` URL for an `<img>`, where it can run nothing.
pub fn svg_data_url(svg: &str) -> String {
    format!("data:image/svg+xml;base64,{}", STANDARD.encode(svg))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_invoice_becomes_an_svg_qr_code() {
        let invoice = "lnbcrt52500n1pnexample0dqqcqzzsxqyz5vqsp5example9qyyssq";
        let svg = lightning_invoice_svg(invoice).unwrap();
        assert!(svg.starts_with(r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 "#));
        assert!(svg.contains(r#"fill="#000""#));
        assert!(!svg.contains("<script"));
        let url = svg_data_url(&svg);
        assert!(url.starts_with("data:image/svg+xml;base64,PHN2Zy"));
    }
}
