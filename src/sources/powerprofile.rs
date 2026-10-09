//! The power profile, from power-profiles-daemon's ActiveProfile. Taken
//! on its PropertiesChanged.
//!
//! {profile: "balanced" | "power-saver" | "performance"}

use super::dbus;
use anyhow::Result;
use serde_json::{json, Value};

pub async fn take() -> Result<Value> {
    let conn = dbus::system().await?;
    let profile = dbus::string(
        dbus::get(
            &conn,
            "org.freedesktop.UPower.PowerProfiles",
            "/org/freedesktop/UPower/PowerProfiles",
            "org.freedesktop.UPower.PowerProfiles",
            "ActiveProfile",
        )
        .await
        .as_ref(),
    );
    Ok(json!({"profile": profile}))
}
