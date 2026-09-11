use std::collections::HashSet;

use freya::prelude::*;
use iroh::EndpointId;

use crate::Model;
use crate::audio_devices;
use crate::persistence::{PairedPeer, Role, WifiCredentials, WifiSecurity};

/// Live discovery + paired-peer state surfaced to the settings dialog. Part of
/// the root [`Model`], so the dialog re-renders whenever pairing, approval,
/// remove, forget or discovery touches it.
pub struct PairingState {
    pub paired_primary: Option<PairedPeer>,
    pub paired_reflections: Vec<PairedPeer>,
    pub discovered_primaries: HashSet<EndpointId>,
}

/// Sentinel rendered as the first option in the audio device dropdown to mean
/// "let cpal/rodio pick the system default each time."
const SYSTEM_DEFAULT: &str = "System default";

const MUTED: (u8, u8, u8, u8) = (255, 255, 255, 140);

pub struct DialogValues {
    pub wifi: WifiCredentials,
    /// Selected output device name, or `None` when "System default" is chosen.
    pub audio_device: Option<String>,
    pub role: Role,
    pub hide_titlebar: bool,
}

/// The settings form. Render it inside a `Popup` only while `open` is true:
/// form fields are seeded from the model when this component mounts, and
/// closing the popup unmounts it, so reopening starts from the saved values.
/// Pairing actions apply immediately; everything else waits for Save.
#[derive(PartialEq)]
pub struct SettingsDialog {
    pub model: State<Model>,
    pub open: State<bool>,
}

impl Component for SettingsDialog {
    fn render(&self) -> impl IntoElement {
        let model = self.model;
        let open = self.open;
        let platform = Platform::get();

        let ssid = use_state(|| {
            model
                .peek()
                .wifi_creds
                .as_ref()
                .map(|c| c.ssid.clone())
                .unwrap_or_default()
        });
        let password = use_state(|| {
            model
                .peek()
                .wifi_creds
                .as_ref()
                .map(|c| c.password.clone())
                .unwrap_or_default()
        });
        let mut security = use_state(|| {
            model
                .peek()
                .wifi_creds
                .as_ref()
                .map(|c| c.security.clone())
                .unwrap_or(WifiSecurity::Wpa)
        });
        let audio_devices = use_hook(audio_devices::output_device_names);
        let mut audio_device = use_state(|| model.peek().db.audio_device_name().ok().flatten());
        let mut role = use_state(|| model.peek().role);
        let mut hide_titlebar = use_state(|| model.peek().hide_titlebar);
        let mut show_password = use_state(|| false);

        let current_role = *role.read();
        let (paired_primary, paired_reflections, discovered) = {
            let m = model.read();
            let mut discovered: Vec<EndpointId> =
                m.pairing.discovered_primaries.iter().copied().collect();
            discovered.sort_by_key(|id| id.fmt_short().to_string());
            (
                m.pairing.paired_primary.clone(),
                m.pairing.paired_reflections.clone(),
                discovered,
            )
        };

        let role_tile = |value: Role, text: &'static str| {
            Tile::new()
                .on_select(move |_| role.set(value))
                .child(RadioItem::new().selected(current_role == value))
                .child(text)
        };

        let security_options = [
            ("WPA", WifiSecurity::Wpa),
            ("Open (no password)", WifiSecurity::None),
        ];
        let current_security = security.read().clone();
        let security_label = security_options
            .iter()
            .find(|(_, s)| *s == current_security)
            .map(|(name, _)| *name)
            .unwrap_or("WPA");

        let current_audio = audio_device.read().clone();
        let audio_label = current_audio
            .clone()
            .unwrap_or_else(|| SYSTEM_DEFAULT.to_string());
        let audio_options: Vec<Option<String>> = std::iter::once(None)
            .chain(audio_devices.iter().cloned().map(Some))
            .collect();

        let show = *show_password.read();

        let form = rect()
            .width(Size::fill())
            .spacing(16.)
            .child(field(
                "Role",
                rect()
                    .horizontal()
                    .child(role_tile(Role::Primary, "Primary"))
                    .child(role_tile(Role::Reflection, "Reflection")),
            ))
            .child(field(
                "Network name (SSID)",
                Input::new(ssid).placeholder("MyNetwork").width(Size::fill()),
            ))
            .child(field(
                "Password",
                Input::new(password)
                    .width(Size::fill())
                    .mode(if show {
                        InputMode::Shown
                    } else {
                        InputMode::new_password()
                    })
                    .trailing(
                        Button::new()
                            .flat()
                            .compact()
                            .on_press(move |_| show_password.toggle())
                            .child(if show { "Hide" } else { "Show" }),
                    ),
            ))
            .child(field(
                "Security type",
                Select::new()
                    .selected_item(security_label)
                    .children(security_options.iter().map(|(name, value)| {
                        let value = value.clone();
                        MenuItem::new()
                            .selected(value == current_security)
                            .on_press(move |_| security.set(value.clone()))
                            .child(*name)
                            .into()
                    })),
            ))
            .child(field(
                "Audio output",
                Select::new()
                    .selected_item(audio_label)
                    .children(audio_options.into_iter().map(|option| {
                        let text = option
                            .clone()
                            .unwrap_or_else(|| SYSTEM_DEFAULT.to_string());
                        MenuItem::new()
                            .selected(option == current_audio)
                            .on_press(move |_| audio_device.set(option.clone()))
                            .child(text)
                            .into()
                    })),
            ))
            .child(
                rect()
                    .horizontal()
                    .spacing(12.)
                    .cross_align(Alignment::Center)
                    .child(
                        Switch::new()
                            .toggled(hide_titlebar)
                            .on_toggle(move |_| hide_titlebar.toggle()),
                    )
                    .child("Hide titlebar"),
            )
            .maybe_child((current_role == Role::Reflection).then(|| {
                pairing_section(model, discovered, paired_primary)
            }))
            .maybe_child(
                (current_role == Role::Primary && !paired_reflections.is_empty())
                    .then(|| reflections_section(model, paired_reflections)),
            );

        let cancel = Button::new()
            .outline()
            .on_press(move |_| {
                let mut open = open;
                open.set(false);
            })
            .child("Cancel");

        let save = Button::new()
            .filled()
            .on_press(move |_| {
                // A remembered device that has since disappeared falls back
                // to the system default rather than being written back.
                let audio_device = audio_device
                    .read()
                    .clone()
                    .filter(|name| audio_devices.contains(name));
                crate::save_settings(
                    model,
                    DialogValues {
                        wifi: WifiCredentials {
                            ssid: ssid.read().clone(),
                            password: password.read().clone(),
                            security: security.read().clone(),
                        },
                        audio_device,
                        role: *role.read(),
                        hide_titlebar: *hide_titlebar.read(),
                    },
                    &platform,
                );
                let mut open = open;
                open.set(false);
            })
            .child("Save");

        rect()
            .width(Size::fill())
            .child(PopupTitle::new("Settings".to_string()))
            .child(PopupContent::new().child(form))
            .child(PopupButtons::new().child(cancel).child(save))
    }
}

fn field(title: &'static str, control: impl Into<Element>) -> Rect {
    rect()
        .width(Size::fill())
        .spacing(4.)
        .child(title)
        .child(control)
}

fn pairing_section(
    model: State<Model>,
    discovered: Vec<EndpointId>,
    paired: Option<PairedPeer>,
) -> Rect {
    let body: Element = if let Some(p) = paired {
        peer_row(
            "Paired with".to_string(),
            p.endpoint_id,
            Button::new()
                .outline()
                .on_press(move |_| crate::forget_primary(model))
                .child("Forget"),
        )
        .into()
    } else if discovered.is_empty() {
        label()
            .color(MUTED)
            .font_size(13.)
            .text("Searching for primaries on this network…")
            .into()
    } else {
        rect()
            .width(Size::fill())
            .spacing(8.)
            .children(discovered.into_iter().map(|id| {
                rect()
                    .horizontal()
                    .width(Size::fill())
                    .main_align(Alignment::SpaceBetween)
                    .cross_align(Alignment::Center)
                    .child(label().font_size(13.).text(id.fmt_short().to_string()))
                    .child(
                        Button::new()
                            .filled()
                            .on_press(move |_| crate::pair_with(model, id))
                            .child("Pair"),
                    )
                    .into()
            }))
            .into()
    };

    rect()
        .width(Size::fill())
        .spacing(8.)
        .child("Primary")
        .child(body)
}

fn reflections_section(model: State<Model>, peers: Vec<PairedPeer>) -> Rect {
    rect()
        .width(Size::fill())
        .spacing(8.)
        .child("Reflections")
        .children(peers.into_iter().map(|peer| {
            let id = peer.endpoint_id;
            peer_row(
                peer.friendly_name,
                id,
                Button::new()
                    .outline()
                    .on_press(move |_| crate::remove_reflection(model, id))
                    .child("Remove"),
            )
            .into()
        }))
}

fn peer_row(name: String, id: EndpointId, action: Button) -> Rect {
    rect()
        .horizontal()
        .width(Size::fill())
        .main_align(Alignment::SpaceBetween)
        .cross_align(Alignment::Center)
        .child(
            rect()
                .spacing(2.)
                .child(name)
                .child(
                    label()
                        .font_size(13.)
                        .color(MUTED)
                        .text(id.fmt_short().to_string()),
                ),
        )
        .child(action)
}
