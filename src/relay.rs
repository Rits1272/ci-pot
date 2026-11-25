//! Fetching events from a relay.
//!
//! Relays cap how many events one request returns, so a fetch walks backwards
//! in time: ask for the newest page, then ask again for everything up to the
//! oldest event seen, until the relay has nothing older to give.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use nostr::prelude::*;
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

/// Events asked for per request. Relays commonly cap at 500.
const PAGE: usize = 500;
/// How long to wait for a relay to say anything.
const PATIENCE: Duration = Duration::from_secs(30);

/// Fetches every event of `kinds` created at or after `since`, calling `found` for each.
///
/// Signatures are not checked here. Returns how many distinct events the relay sent.
pub async fn fetch(
    relay: &str,
    kinds: &[u16],
    since: u64,
    mut found: impl FnMut(Event) -> Result<()>,
) -> Result<usize> {
    let (mut socket, _) = tokio::time::timeout(PATIENCE, tokio_tungstenite::connect_async(relay))
        .await
        .map_err(|_| anyhow!("{relay} did not answer"))?
        .with_context(|| format!("cannot connect to {relay}"))?;

    let mut seen: HashSet<EventId> = HashSet::new();
    let mut until: Option<u64> = None;

    for page in 0.. {
        let subscription = format!("ci-pot-{page}");
        let mut filter = json!({ "kinds": kinds, "since": since, "limit": PAGE });
        if let Some(until) = until {
            filter["until"] = json!(until);
        }
        let request = json!(["REQ", subscription, filter]).to_string();
        socket.send(Message::Text(request.into())).await?;

        // Read this page until the relay says it has sent everything stored.
        let mut new_in_page = 0;
        let mut oldest = u64::MAX;
        loop {
            let message = tokio::time::timeout(PATIENCE, socket.next())
                .await
                .map_err(|_| anyhow!("{relay} stopped answering"))?
                .ok_or_else(|| anyhow!("{relay} closed the connection"))??;
            let Message::Text(text) = message else {
                continue;
            };
            let Ok(Value::Array(parts)) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            match parts.first().and_then(Value::as_str) {
                Some("EVENT") if parts.len() >= 3 => {
                    let Ok(event) = Event::from_json(parts[2].to_string()) else {
                        continue;
                    };
                    oldest = oldest.min(event.created_at.as_secs());
                    if seen.insert(event.id) {
                        new_in_page += 1;
                        found(event)?;
                    }
                }
                Some("EOSE") => break,
                Some("CLOSED") => {
                    bail!(
                        "{relay} refused the request: {}",
                        parts.last().unwrap_or(&Value::Null)
                    )
                }
                _ => {}
            }
        }
        socket
            .send(Message::Text(
                json!(["CLOSE", subscription]).to_string().into(),
            ))
            .await?;

        // A page with nothing new means the relay has nothing older.
        if new_in_page == 0 {
            break;
        }
        // Ask again from the oldest timestamp, inclusive: several events can
        // share one second, and the ones already seen are skipped by id.
        until = Some(oldest);
    }

    let _ = socket.close(None).await;
    Ok(seen.len())
}
