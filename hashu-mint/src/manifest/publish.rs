//! Live relay publishing for pre-signed nostr events.
//!
//! Thin wrapper over `nostr_sdk::Client::send_event` that surfaces per-relay
//! success/failure to the caller. The event must already be signed —
//! [`super::event::build_event`] handles that synchronously.

use nostr::{Event, EventId};
use nostr_sdk::Client;

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("nostr-sdk client: {0}")]
    Client(#[from] nostr_sdk::client::Error),
    #[error("relay url {url}: {source}")]
    RelayUrl {
        url: String,
        source: nostr_sdk::client::Error,
    },
}

#[derive(Debug, Clone)]
pub struct PublishOutcome {
    pub event_id: EventId,
    /// Relay URLs that ack'd the event.
    pub succeeded: Vec<String>,
    /// Relay URLs that rejected the event, with the relay's error message.
    pub failed: Vec<(String, String)>,
}

/// Publish `event` to each relay in `relays`. Returns aggregated per-relay
/// success/failure once all relays have responded (or hit their timeout).
pub async fn publish(event: &Event, relays: &[String]) -> Result<PublishOutcome, PublishError> {
    let client = Client::builder().build();

    for url in relays {
        client
            .add_write_relay(url)
            .await
            .map_err(|source| PublishError::RelayUrl {
                url: url.clone(),
                source,
            })?;
    }

    client.connect().await;
    let output = client.send_event(event).await?;
    let _ = client.disconnect().await;

    let succeeded = output.success.iter().map(|u| u.to_string()).collect();
    let failed = output
        .failed
        .iter()
        .map(|(u, e)| (u.to_string(), e.clone()))
        .collect();

    Ok(PublishOutcome {
        event_id: output.val,
        succeeded,
        failed,
    })
}
