//! Construction of the per-process [`iroh::Endpoint`].
//!
//! Identity is symmetric across roles — primary and reflection both bind the
//! same way. The persisted `SecretKey` from `persistence::Database::node_secret`
//! is what makes the EndpointId stable across restarts.
//!
//! mDNS discovery is attached unconditionally so a primary advertises and a
//! reflection sees it without any role-specific endpoint configuration. The
//! discovery filter (T10) keys on iroh's `UserData` field set below.

use iroh::{
    Endpoint, SecretKey, address_lookup::MdnsAddressLookup, endpoint::presets,
    endpoint_info::UserData,
};

use super::ALPN;

/// Tag advertised over mDNS so reflections can filter for our advertisements
/// rather than every iroh endpoint on the LAN.
pub const USER_DATA_TAG: &str = "xyz.mooshq.guest-info-display";

/// Build an endpoint bound to `secret_key`, with mDNS discovery attached.
/// Returns `None` (after logging) if the endpoint cannot bind — multi-screen
/// degrades cleanly when the network stack is unavailable. The second tuple
/// element is the live `MdnsAddressLookup` handle so the caller can subscribe
/// to discovery events.
pub async fn build(secret_key: SecretKey) -> Option<(Endpoint, Option<MdnsAddressLookup>)> {
    let endpoint = match Endpoint::builder(presets::N0)
        .secret_key(secret_key)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await
    {
        Ok(e) => e,
        Err(e) => {
            log::warn!("multi_screen: endpoint bind failed, multi-screen disabled: {e}");
            return None;
        }
    };

    let mdns = match MdnsAddressLookup::builder().build(endpoint.id()) {
        Ok(mdns) => match endpoint.address_lookup() {
            Ok(al) => {
                al.add(mdns.clone());
                Some(mdns)
            }
            Err(e) => {
                log::warn!("multi_screen: address lookup unavailable: {e}");
                None
            }
        },
        Err(e) => {
            log::warn!("multi_screen: mdns init failed: {e}");
            None
        }
    };

    if let Ok(user_data) = UserData::try_from(USER_DATA_TAG.to_string()) {
        endpoint.set_user_data_for_address_lookup(Some(user_data));
    }

    Some((endpoint, mdns))
}
