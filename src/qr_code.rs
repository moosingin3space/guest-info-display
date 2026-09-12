use freya::engine::prelude::{Paint, SkRect};
use freya::prelude::*;
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
/// The returned element should be placed inside a fixed-size container.
/// Falls back to a text placeholder if QR generation fails.
pub fn wifi_qr_element(creds: &WifiCredentials) -> Element {
    let Ok(qr) = QrCode::encode_text(&wifi_string(creds), QrCodeEcc::Medium) else {
        return rect().expanded().center().child("QR unavailable").into();
    };

    canvas(RenderCallback::new(move |ctx: &mut CanvasContext| {
        let n = qr.size() as f32;
        let w = ctx.size.width;
        let h = ctx.size.height;
        let module_px = w.min(h) / n;

        // Anti-aliasing would leave hairline seams between adjacent modules.
        let mut paint = Paint::default();
        paint.set_anti_alias(false);

        paint.set_color(Color::WHITE);
        ctx.canvas.draw_rect(SkRect::from_xywh(0., 0., w, h), &paint);

        paint.set_color(Color::BLACK);
        for row in 0..qr.size() {
            for col in 0..qr.size() {
                if qr.get_module(col, row) {
                    ctx.canvas.draw_rect(
                        SkRect::from_xywh(
                            col as f32 * module_px,
                            row as f32 * module_px,
                            module_px,
                            module_px,
                        ),
                        &paint,
                    );
                }
            }
        }
    }))
    .expanded()
    .into()
}
