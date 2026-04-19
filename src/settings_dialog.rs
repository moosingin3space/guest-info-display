use std::sync::Arc;

use gpui::{App, Window, prelude::*, px};
use gpui_component::{
    IndexPath, WindowExt,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    select::{Select, SelectState},
    v_flex,
};

use crate::persistence::{WifiCredentials, WifiSecurity};

/// Opens the Wi-Fi settings dialog.
///
/// `existing` is used to pre-fill the form fields.
/// `on_save` is called with the validated credentials when the user confirms.
pub fn open(
    existing: Option<WifiCredentials>,
    on_save: Arc<dyn Fn(WifiCredentials, &mut App) + 'static>,
    window: &mut Window,
    cx: &mut App,
) {
    let ssid_input = cx.new(|cx| {
        let state = InputState::new(window, cx).placeholder("MyNetwork");
        if let Some(ref creds) = existing {
            state.default_value(creds.ssid.clone())
        } else {
            state
        }
    });

    let password_input = cx.new(|cx| {
        let state = InputState::new(window, cx).masked(true);
        if let Some(ref creds) = existing {
            state.default_value(creds.password.clone())
        } else {
            state
        }
    });

    let initial_idx = match existing.as_ref().map(|c| &c.security) {
        Some(WifiSecurity::None) => IndexPath::new(1),
        _ => IndexPath::new(0),
    };
    let security_select = cx.new(|cx| {
        SelectState::new(
            vec!["WPA", "Open (no password)"],
            Some(initial_idx),
            window,
            cx,
        )
    });

    window.open_dialog(cx, move |dialog, _, _| {
        // Clone handles for rendering the form fields.
        let ssid_render = ssid_input.clone();
        let pwd_render = password_input.clone();
        let sec_render = security_select.clone();

        // Separate clones captured by the footer Fn closure.
        let ssid_footer = ssid_input.clone();
        let pwd_footer = password_input.clone();
        let sec_footer = security_select.clone();
        let on_save_footer = on_save.clone();

        dialog
            .title("Wi-Fi Settings")
            .w(px(420.))
            .child(
                v_flex()
                    .gap_4()
                    .py_2()
                    .child(
                        v_flex()
                            .gap_1()
                            .child("Network name (SSID)")
                            .child(Input::new(&ssid_render)),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child("Password")
                            .child(Input::new(&pwd_render).mask_toggle()),
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child("Security type")
                            .child(Select::new(&sec_render)),
                    ),
            )
            .footer(move |_, _, _, _| {
                // Clone once per footer render so the save on_click can move them.
                let ssid = ssid_footer.clone();
                let password = pwd_footer.clone();
                let security = sec_footer.clone();
                let on_save = on_save_footer.clone();

                vec![
                    Button::new("cancel")
                        .outline()
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx))
                        .into_any_element(),
                    Button::new("save")
                        .primary()
                        .label("Save")
                        .on_click(move |_, window, cx| {
                            let ssid_val = ssid.read(cx).value().to_string();
                            let pwd_val = password.read(cx).value().to_string();
                            let sec_val =
                                match security.read(cx).selected_index(cx).map(|ix| ix.row) {
                                    Some(1) => WifiSecurity::None,
                                    _ => WifiSecurity::Wpa,
                                };
                            on_save(
                                WifiCredentials {
                                    ssid: ssid_val,
                                    password: pwd_val,
                                    security: sec_val,
                                },
                                cx,
                            );
                            window.close_dialog(cx);
                        })
                        .into_any_element(),
                ]
            })
    });
}
