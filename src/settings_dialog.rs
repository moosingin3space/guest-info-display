use std::cell::Cell;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, Entity, IntoElement, SharedString, Window, div, hsla, prelude::*, px};
use gpui_component::{
    IndexPath, WindowExt,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState},
    radio::RadioGroup,
    select::{Select, SelectState},
    switch::Switch,
    v_flex,
};
use iroh::EndpointId;

use crate::persistence::{PairedPeer, Role, WifiCredentials, WifiSecurity};

/// Live data the settings dialog reads while open. Held in a sub-entity of
/// `GuestInfoDisplay` so the dialog builder (which runs during the parent's
/// render and therefore can't reach back into it) can read fresh state on
/// every frame, and so that mutation paths funnel through `pairing.update`
/// without runtime-checked interior mutability.
pub struct PairingState {
    pub paired_primary: Option<PairedPeer>,
    pub paired_reflections: Vec<PairedPeer>,
    pub discovered_primaries: HashSet<EndpointId>,
}

pub type OnPairFn = Arc<dyn Fn(EndpointId, &mut App) + 'static>;
pub type OnRemovePeerFn = Arc<dyn Fn(EndpointId, &mut App) + 'static>;
pub type OnForgetPrimaryFn = Arc<dyn Fn(&mut App) + 'static>;

/// Sentinel rendered as the first option in the audio device dropdown to mean
/// "let cpal/rodio pick the system default each time."
const SYSTEM_DEFAULT: &str = "System default";

pub struct DialogValues {
    pub wifi: WifiCredentials,
    /// Selected output device name, or `None` when "System default" is chosen.
    pub audio_device: Option<String>,
    pub role: Role,
    pub hide_titlebar: bool,
}

pub type OnSaveFn = Arc<dyn Fn(DialogValues, &mut App) + 'static>;

/// Opens the settings dialog.
///
/// Pre-fills the form fields from the supplied values.
/// `audio_devices` is the list of cpal output device names to offer.
/// `pairing` carries the live discovery / paired-peer state; the dialog
/// builder re-reads it on every frame so peer-affecting actions (pair,
/// approval, remove, forget) update the visible content in place.
/// `on_save` is called with the form values when the user confirms.
/// `on_pair` is fired when the user clicks Pair next to a discovered primary.
pub struct SettingsDialog<'a, 'b> {
    pub existing_wifi: Option<WifiCredentials>,
    pub existing_audio: Option<String>,
    pub existing_role: Role,
    pub existing_hide_titlebar: bool,
    pub audio_devices: Vec<String>,
    pub pairing: Entity<PairingState>,
    pub on_save: OnSaveFn,
    pub on_pair: OnPairFn,
    pub on_remove_reflection: OnRemovePeerFn,
    pub on_forget_primary: OnForgetPrimaryFn,
    pub window: &'a mut Window,
    pub cx: &'b mut App,
}

impl<'a, 'b> SettingsDialog<'a, 'b> {
    pub fn run(self) {
        let SettingsDialog {
            existing_wifi,
            existing_audio,
            existing_role,
            existing_hide_titlebar,
            audio_devices,
            pairing,
            on_save,
            on_pair,
            on_remove_reflection,
            on_forget_primary,
            window,
            cx,
        } = self;

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
        let audio_select =
            cx.new(|cx| SelectState::new(audio_options, audio_initial_idx, window, cx));

        // Role isn't backed by a stateful entity (RadioGroup is a render-only
        // element), so we pin it through an `Rc<Cell<_>>` so the on_click handler
        // can write the new selection that the Save button reads.
        let role_state = Rc::new(Cell::new(existing_role));
        let hide_titlebar_state = Rc::new(Cell::new(existing_hide_titlebar));

        window.open_dialog(cx, move |dialog, _, cx| {
            let ssid_render = ssid_input.clone();
            let pwd_render = password_input.clone();
            let sec_render = security_select.clone();
            let audio_render = audio_select.clone();
            let role_render = role_state.clone();
            let role_handler = role_state.clone();
            let hide_titlebar_render = hide_titlebar_state.clone();
            let hide_titlebar_handler = hide_titlebar_state.clone();

            let ssid_footer = ssid_input.clone();
            let pwd_footer = password_input.clone();
            let sec_footer = security_select.clone();
            let audio_footer = audio_select.clone();
            let audio_options_footer = audio_options_for_lookup.clone();
            let role_footer = role_state.clone();
            let hide_titlebar_footer = hide_titlebar_state.clone();
            let on_save_footer = on_save.clone();

            let current_role = role_render.get();
            // Reading `pairing` here registers it as a render-time dependency;
            // any `pairing.update(cx, …)` from a callback (pair, approval,
            // remove, forget, discovery) automatically re-runs this builder.
            let (paired_primary_now, paired_reflections_now, discovered_sorted) = {
                let s = pairing.read(cx);
                let mut discovered: Vec<EndpointId> =
                    s.discovered_primaries.iter().copied().collect();
                discovered.sort_by_key(|id| id.fmt_short().to_string());
                (
                    s.paired_primary.clone(),
                    s.paired_reflections.clone(),
                    discovered,
                )
            };
            let pair_section = (current_role == Role::Reflection).then(|| {
                pairing_section(
                    discovered_sorted,
                    paired_primary_now,
                    on_pair.clone(),
                    on_forget_primary.clone(),
                )
            });
            let reflections_section = (current_role == Role::Primary
                && !paired_reflections_now.is_empty())
            .then(|| reflections_section(paired_reflections_now, on_remove_reflection.clone()));

            dialog
                .title("Settings")
                .w(px(420.))
                .child(
                    v_flex()
                        .gap_4()
                        .py_2()
                        .child(
                            v_flex().gap_1().child("Role").child(
                                RadioGroup::horizontal("role")
                                    .selected_index(Some(match role_render.get() {
                                        Role::Primary => 0,
                                        Role::Reflection => 1,
                                    }))
                                    .children(["Primary", "Reflection"])
                                    .on_click(move |ix, window, _| {
                                        role_handler.set(match *ix {
                                            0 => Role::Primary,
                                            _ => Role::Reflection,
                                        });
                                        // Force the dialog builder to re-run so
                                        // the radio's selected indicator and the
                                        // role-conditional sections (pair /
                                        // reflections) update without needing a
                                        // dismiss-and-reopen.
                                        window.refresh();
                                    }),
                            ),
                        )
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
                        )
                        .child(
                            Switch::new("hide-titlebar")
                                .label("Hide titlebar")
                                .checked(hide_titlebar_render.get())
                                .on_click(move |checked, window, _| {
                                    hide_titlebar_handler.set(*checked);
                                    window.refresh();
                                }),
                        )
                        .when_some(pair_section, |el, section| el.child(section))
                        .when_some(reflections_section, |el, section| el.child(section)),
                )
                .footer(move |_, _, _, _| {
                    let ssid = ssid_footer.clone();
                    let password = pwd_footer.clone();
                    let security = sec_footer.clone();
                    let audio = audio_footer.clone();
                    let audio_options = audio_options_footer.clone();
                    let role = role_footer.clone();
                    let hide_titlebar = hide_titlebar_footer.clone();
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
                                        role: role.get(),
                                        hide_titlebar: hide_titlebar.get(),
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
}

fn pairing_section(
    discovered: Vec<EndpointId>,
    paired: Option<PairedPeer>,
    on_pair: OnPairFn,
    on_forget: OnForgetPrimaryFn,
) -> impl IntoElement {
    let muted = hsla(0.0, 0.0, 1.0, 0.55);

    v_flex().gap_2().child("Primary").child(if let Some(p) = paired {
        h_flex()
            .justify_between()
            .items_center()
            .gap_3()
            .child(
                v_flex()
                    .gap_0p5()
                    .child(div().child("Paired with"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(muted)
                            .child(format!("{}", p.endpoint_id.fmt_short())),
                    ),
            )
            .child(
                Button::new("forget-primary")
                    .outline()
                    .label("Forget")
                    .on_click(move |_, _, cx| {
                        on_forget(cx);
                    }),
            )
            .into_any_element()
    } else if discovered.is_empty() {
        div()
            .text_color(muted)
            .text_sm()
            .child("Searching for primaries on this network…")
            .into_any_element()
    } else {
        v_flex()
            .gap_2()
            .children(discovered.into_iter().map(|id| {
                let on_pair = on_pair.clone();
                h_flex()
                    .justify_between()
                    .items_center()
                    .gap_3()
                    .child(div().text_sm().child(format!("{}", id.fmt_short())))
                    .child(
                        Button::new(SharedString::from(format!("pair-{}", id.fmt_short())))
                            .primary()
                            .label("Pair")
                            .on_click(move |_, _, cx| {
                                on_pair(id, cx);
                            }),
                    )
            }))
            .into_any_element()
    })
}

fn reflections_section(
    peers: Vec<PairedPeer>,
    on_remove: OnRemovePeerFn,
) -> impl IntoElement {
    let muted = hsla(0.0, 0.0, 1.0, 0.55);

    v_flex()
        .gap_2()
        .child("Reflections")
        .children(peers.into_iter().map(|peer| {
            let on_remove = on_remove.clone();
            let id = peer.endpoint_id;
            h_flex()
                .justify_between()
                .items_center()
                .gap_3()
                .child(
                    v_flex()
                        .gap_0p5()
                        .child(div().child(peer.friendly_name.clone()))
                        .child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("{}", id.fmt_short())),
                        ),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-{}", id.fmt_short())))
                        .outline()
                        .label("Remove")
                        .on_click(move |_, _, cx| {
                            on_remove(id, cx);
                        }),
                )
        }))
}
