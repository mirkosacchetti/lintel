//! The bar's own notifications: posted straight to the bar's daemon when
//! it runs (sources/notifyd.rs), over the bus to whatever daemon owns
//! org.freedesktop.Notifications otherwise.

use super::notifyd::{self, ActionHandler, Internal};
use anyhow::Result;
use std::collections::HashMap;
use zbus::zvariant::Value;

#[derive(Clone, Copy)]
pub enum Urgency {
    Normal = 1,
    Critical = 2,
}

/// Never goes away on its own: the daemon keeps it until you close it.
pub const STICKY: i32 = 0;
/// The daemon's default timeout.
pub const DEFAULT: i32 = -1;

pub async fn send(summary: &str, body: &str, urgency: Urgency, timeout_ms: i32) -> Result<u32> {
    let conn = zbus::Connection::session().await?;
    let mut hints: HashMap<&str, Value> = HashMap::new();
    hints.insert("urgency", Value::U8(urgency as u8));
    let reply = conn
        .call_method(
            Some("org.freedesktop.Notifications"),
            "/org/freedesktop/Notifications",
            Some("org.freedesktop.Notifications"),
            "Notify",
            &("lintel", 0u32, "", summary, body, Vec::<&str>::new(), hints, timeout_ms),
        )
        .await?;
    let id: u32 = reply.body().deserialize()?;
    Ok(id)
}

/// Fire and forget, with the daemon's timeout.
pub fn spawn(summary: String, body: String, urgency: Urgency) {
    spawn_with(summary, body, urgency, DEFAULT);
}

pub fn spawn_with(summary: String, body: String, urgency: Urgency, timeout_ms: i32) {
    spawn_actions(summary, body, urgency, timeout_ms, Vec::new(), None);
}

/// With buttons: `actions` are (key, label), `on_action` gets the key.
/// Buttons need the bar's own daemon; over the bus they are dropped.
pub fn spawn_actions(
    summary: String,
    body: String,
    urgency: Urgency,
    timeout_ms: i32,
    actions: Vec<(String, String)>,
    on_action: Option<ActionHandler>,
) {
    let posted = notifyd::post(Internal {
        summary: summary.clone(),
        body: body.clone(),
        urgency: urgency as u8,
        timeout_ms,
        icon: String::new(),
        actions,
        on_action,
    });
    if posted {
        return;
    }
    tokio::spawn(async move {
        if let Err(e) = send(&summary, &body, urgency, timeout_ms).await {
            eprintln!("notify: {e}");
        }
    });
}
