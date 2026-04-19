use gpui::{AnyElement, Bounds, IntoElement, Point, Size, Window, black, canvas, div, fill, prelude::*, px, white};
use qrcodegen::{QrCode, QrCodeEcc};

use crate::persistence::{WifiCredentials, WifiSecurity};

/// Returns the ZXing-format Wi-Fi QR string for the given credentials.
/// Special characters in SSID and password are escaped per the spec.
fn wifi_string(creds: &WifiCredentials) -> String {
    if creds.security == WifiSecurity::None {
        format!("WIFI:T:nopass;S:{};;", escape_field(&creds.ssid))
    } else {
        format!(
            "WIFI:T:{};S:{};P:{};;",
            creds.security.qr_type_str(),
            escape_field(&creds.ssid),
            escape_field(&creds.password),
        )
    }
}

/// Escapes characters that have special meaning in the Wi-Fi QR string format.
fn escape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(ch, '\\' | ';' | ',' | '"') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Renders a Wi-Fi QR code that fills its parent element.
/// The returned element should be placed inside a fixed-size div.
/// Falls back to a text placeholder if QR generation fails.
pub fn wifi_qr_element(creds: &WifiCredentials) -> AnyElement {
    let Ok(qr) = QrCode::encode_text(&wifi_string(creds), QrCodeEcc::Medium) else {
        return div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child("QR unavailable")
            .into_any_element();
    };

    canvas(
        |_, _, _| {},
        move |bounds, _, window: &mut Window, _| {
            let n = qr.size() as f32;
            let w = f32::from(bounds.size.width);
            let h = f32::from(bounds.size.height);
            let module_px = w.min(h) / n;
            let ox = f32::from(bounds.origin.x);
            let oy = f32::from(bounds.origin.y);

            window.paint_quad(fill(bounds, white()));

            for row in 0..qr.size() {
                for col in 0..qr.size() {
                    if qr.get_module(col, row) {
                        window.paint_quad(fill(
                            Bounds {
                                origin: Point {
                                    x: px(ox + col as f32 * module_px),
                                    y: px(oy + row as f32 * module_px),
                                },
                                size: Size {
                                    width: px(module_px),
                                    height: px(module_px),
                                },
                            },
                            black(),
                        ));
                    }
                }
            }
        },
    )
    .size_full()
    .into_any_element()
}
