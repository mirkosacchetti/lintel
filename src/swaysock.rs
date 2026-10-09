//! The sway IPC socket. SWAYSOCK names it, but a bar that systemd brings
//! back right after sway itself crashed starts before the new sway has
//! exported its variables, and holds the dead one's path. So the path is
//! checked before use, and when it does not answer the live socket is
//! looked for in the runtime directory, the newest first. Connections and
//! the commands run through swaymsg both take it from here.

use anyhow::{anyhow, Result};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Mutex;

static CACHE: Mutex<Option<PathBuf>> = Mutex::new(None);

fn alive(p: &PathBuf) -> bool {
    UnixStream::connect(p).is_ok()
}

/// sway-ipc.UID.PID.sock in the runtime directory, the newest first.
fn candidates() -> Vec<PathBuf> {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let mut found: Vec<(std::time::SystemTime, PathBuf)> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("sway-ipc.") && name.ends_with(".sock")
        })
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    found.sort_by(|a, b| b.0.cmp(&a.0));
    found.into_iter().map(|(_, p)| p).collect()
}

/// The socket of the running sway, if there is one.
pub fn path() -> Option<PathBuf> {
    let mut cached = CACHE.lock().unwrap();
    if let Some(p) = cached.as_ref().filter(|p| alive(p)) {
        return Some(p.clone());
    }
    let env = std::env::var_os("SWAYSOCK").map(PathBuf::from);
    let found = env.into_iter().chain(candidates()).find(alive);
    cached.clone_from(&found);
    found
}

pub async fn connect() -> Result<swayipc_async::Connection> {
    let p = path().ok_or_else(|| anyhow!("no sway socket answers"))?;
    let stream = async_io::Async::new(UnixStream::connect(&p)?)?;
    Ok(swayipc_async::Connection::from(stream))
}
