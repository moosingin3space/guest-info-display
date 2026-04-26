use std::sync::Arc;

use gpui::{App, SharedString, Window, prelude::*, px};
use gpui_component::{
    IndexPath, WindowExt,
    button::{Button, ButtonVariants},
    input::{Input, InputState},
    select::{Select, SelectState},
    v_flex,
};

use crate::persistence::{WifiCredentials, WifiSecurity};

/// Sentinel rendered as the first option in the audio device dropdown to mean
/// "let cpal/rodio pick the system default each time."
const SYSTEM_DEFAULT: &str = "System default";

pub struct DialogValues {
    pub wifi: WifiCredentials,
    /// Selected output device name, or `None` when "System default" is chosen.
    pub audio_device: Option<String>,
}

/// Opens the settings dialog.
///
/// `existing_wifi` and `existing_audio` pre-fill the form fields.
/// `audio_devices` is the list of cpal output device names to offer.
/// `on_save` is called with the form values when the user confirms.
pub fn open(
    existing_wifi: Option<WifiCredentials>,
    existing_audio: Option<String>,
    audio_devices: Vec<String>,
    on_save: Arc<dyn Fn(DialogValues, &mut App) + 'static>,
    window: &mut Window,
    cx: &mut App,
) {
    let ssid_input = cx.new(|cx| {
        let state = InputState::new(window, cx).placeholder("MyNetwork");
        if let Some(ref creds) = existing_wifi {
            state.default_value(creds.ssid.clone())
        } else {
            state
        }
    });

    let password_input = cx.new(|cx| {
        let state = InputState::new(window, cx).masked(true);
        if let Some(ref creds) = existing_wifi {
            state.default_value(creds.password.clone())
        } else {
            state
        }
    });

    let initial_idx = match existing_wifi.as_ref().map(|c| &c.security) {
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

    // Audio device list: System default + every cpal output device, in order.
    let audio_options: Vec<SharedString> = std::iter::once(SharedString::from(SYSTEM_DEFAULT))
        .chain(audio_devices.into_iter().map(SharedString::from))
        .collect();
    let audio_initial_idx = match existing_audio.as_deref() {
        None => Some(IndexPath::new(0)),
        Some(name) => audio_options
            .iter()
            .position(|opt| opt.as_ref() == name)
            .map(IndexPath::new),
    };
    let audio_options_for_lookup = audio_options.clone();
    let audio_select = cx.new(|cx| SelectState::new(audio_options, audio_initial_idx, window, cx));

    window.open_dialog(cx, move |dialog, _, _| {
        let ssid_render = ssid_input.clone();
        let pwd_render = password_input.clone();
        let sec_render = security_select.clone();
        let audio_render = audio_select.clone();

        let ssid_footer = ssid_input.clone();
        let pwd_footer = password_input.clone();
        let sec_footer = security_select.clone();
        let audio_footer = audio_select.clone();
        let audio_options_footer = audio_options_for_lookup.clone();
        let on_save_footer = on_save.clone();

        dialog
            .title("Settings")
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
                    )
                    .child(
                        v_flex()
                            .gap_1()
                            .child("Audio output")
                            .child(Select::new(&audio_render)),
                    ),
            )
            .footer(move |_, _, _, _| {
                let ssid = ssid_footer.clone();
                let password = pwd_footer.clone();
                let security = sec_footer.clone();
                let audio = audio_footer.clone();
                let audio_options = audio_options_footer.clone();
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
                            let audio_val = audio
                                .read(cx)
                                .selected_index(cx)
                                .and_then(|ix| audio_options.get(ix.row))
                                .filter(|name| name.as_ref() != SYSTEM_DEFAULT)
                                .map(|name| name.to_string());
                            on_save(
                                DialogValues {
                                    wifi: WifiCredentials {
                                        ssid: ssid_val,
                                        password: pwd_val,
                                        security: sec_val,
                                    },
                                    audio_device: audio_val,
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
