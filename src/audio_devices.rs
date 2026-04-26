//! Thin wrapper around `cpal` for enumerating output devices for the settings UI.
//!
//! rodio uses cpal internally for the actual sink. We just expose the same
//! enumeration here so the settings dialog can populate a dropdown.

use cpal::traits::{DeviceTrait, HostTrait};

/// Returns the names of every output device on the default cpal host.
/// Devices that fail name lookup are silently skipped.
pub fn output_device_names() -> Vec<String> {
    let host = cpal::default_host();
    let Ok(devices) = host.output_devices() else {
        return Vec::new();
    };
    devices.filter_map(|d| d.name().ok()).collect()
}
