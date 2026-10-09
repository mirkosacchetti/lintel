//! The two bus connections, opened once and shared by the native
//! sources, and the small calls they all make.

use anyhow::Result;
use std::collections::HashMap;
use tokio::sync::OnceCell;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};
use zbus::Connection;

static SYSTEM: OnceCell<Connection> = OnceCell::const_new();
static SESSION: OnceCell<Connection> = OnceCell::const_new();

pub async fn system() -> Result<Connection> {
    Ok(SYSTEM.get_or_try_init(Connection::system).await?.clone())
}

pub async fn session() -> Result<Connection> {
    Ok(SESSION.get_or_try_init(Connection::session).await?.clone())
}

/// One property, or nothing when the object, the interface or the
/// property is not there.
pub async fn get(conn: &Connection, dest: &str, path: &str, iface: &str, name: &str) -> Option<OwnedValue> {
    let reply = conn
        .call_method(Some(dest), path, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, name))
        .await
        .ok()?;
    reply.body().deserialize::<Value>().ok().and_then(|v| v.try_to_owned().ok())
}

pub type Objects = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;

/// Everything a daemon exports, by path and interface (ObjectManager).
pub async fn managed_objects(conn: &Connection, dest: &str) -> Result<Objects> {
    let reply = conn
        .call_method(
            Some(dest),
            "/",
            Some("org.freedesktop.DBus.ObjectManager"),
            "GetManagedObjects",
            &(),
        )
        .await?;
    Ok(reply.body().deserialize()?)
}

pub fn string(v: Option<&OwnedValue>) -> String {
    v.and_then(|v| String::try_from(v.try_clone().ok()?).ok()).unwrap_or_default()
}

pub fn boolean(v: Option<&OwnedValue>) -> bool {
    v.and_then(|v| bool::try_from(v.try_clone().ok()?).ok()).unwrap_or(false)
}
