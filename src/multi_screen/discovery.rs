//! mDNS-backed discovery loop. Filters the iroh discovery stream to peers that
//! advertise our app's `USER_DATA_TAG` so reflections see only primaries
//! belonging to this app.
//!
//! The loop runs unconditionally on the multi-screen runtime; each instance
//! publishes onto an `async-channel` that the GPUI side drains when the
//! settings dialog is open.

use async_channel::Sender as AsyncSender;
use futures_util::StreamExt;
use iroh::{
    EndpointId,
    address_lookup::{DiscoveryEvent, MdnsAddressLookup},
};

use super::endpoint::USER_DATA_TAG;

#[derive(Debug, Clone)]
pub struct DiscoveredNode {
    pub endpoint_id: EndpointId,
}

pub async fn run(mdns: MdnsAddressLookup, tx: AsyncSender<DiscoveredNode>) {
    let mut stream = mdns.subscribe().await;
    while let Some(event) = stream.next().await {
        let DiscoveryEvent::Discovered { endpoint_info, .. } = event else {
            continue;
        };

        let tag_matches = endpoint_info
            .data
            .user_data()
            .is_some_and(|ud| ud.as_ref() == USER_DATA_TAG);
        if !tag_matches {
            continue;
        }

        let node = DiscoveredNode {
            endpoint_id: endpoint_info.endpoint_id,
        };
        // Best-effort publish: if the GPUI consumer isn't draining, skip
        // rather than block the discovery stream.
        let _ = tx.try_send(node);
    }
}
