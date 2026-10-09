//! A snapshot source: a value taken once at start and again whenever one
//! of its triggers fires (a udev event, a D-Bus signal, a netlink message,
//! a sway event, `lintel refresh`, the card opening). The scripted kind
//! (`command`) and the native kinds (battery, network, ...) share this;
//! only how the value is taken differs.

use super::triggers;
use super::{Hub, Refresh};
use crate::config::Source;
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::time::Duration;
use tokio::sync::mpsc;

pub async fn run<F, Fut>(src: Source, hub: Hub, take: F)
where
    F: Fn(Source, Hub) -> Fut,
    Fut: Future<Output = Result<Value>>,
{
    let (tick_tx, mut tick_rx) = mpsc::unbounded_channel::<&'static str>();
    let mut on_hover = false;
    let mut on_refresh = false;
    for t in &src.triggers {
        match t.as_str() {
            "hover" => on_hover = true,
            "refresh" => on_refresh = true,
            other => {
                if let Err(e) = triggers::spawn(other, tick_tx.clone()) {
                    eprintln!("{}: trigger {other:?}: {e}", src.name);
                }
            }
        }
    }
    if on_hover || on_refresh {
        let mut rx = hub.refresh.subscribe();
        let name = src.name.clone();
        let tx = tick_tx.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Refresh { name: n, hover }) if n == name => {
                        if (hover && on_hover) || (!hover && on_refresh) {
                            let _ = tx.send(if hover { "hover" } else { "refresh" });
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
            }
        });
    }
    drop(tick_tx);

    let coalesce = Duration::from_millis(hub.cfg.bar.coalesce_ms);
    let emit = |v: Result<Value>| match v {
        Ok(v) => hub.emit(&src.name, v),
        Err(e) => eprintln!("{}: {e:#}", src.name),
    };
    emit(take(src.clone(), hub.clone()).await);
    while tick_rx.recv().await.is_some() {
        // a burst of events is one snapshot
        tokio::time::sleep(coalesce).await;
        while tick_rx.try_recv().is_ok() {}
        emit(take(src.clone(), hub.clone()).await);
    }
}
