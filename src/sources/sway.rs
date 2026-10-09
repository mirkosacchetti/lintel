//! Sway, natively: one IPC subscription to workspace, window and mode
//! events, and only what an event touches read back: a workspace event
//! re-reads the workspace list, a title or focus event carries the title
//! in the event itself, a mode event carries the mode, and the tree (the
//! big one) is read only when a window moves or closes, for the
//! scratchpad. A title changing in a terminal costs no IPC call at all.
//!
//! Emits `NAME` = {workspaces: [{name, num, focused, visible, urgent,
//! output}], mode, title} and `scratchpad` = {text, info, class, count}:
//! the scratchpad icon with the count beside it, the windows one per line, and
//! "focused" when one of them has the focus.

use super::Hub;
use crate::config::Source;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde_json::json;
use std::time::Duration;
use swayipc_async::{Connection, Event, EventType, Node, NodeType, ScratchpadState, WindowChange, WorkspaceChange};

/// The scratchpad's icon, checkbox-multiple-blank-outline; the count
/// follows it in subscript digits, smaller than the workspaces' names.
const SCRATCH_ICON: char = '\u{f0137}';

/// `n` in subscript digits (₀ to ₉ are U+2080 to U+2089).
fn subscript(n: usize) -> String {
    n.to_string()
        .chars()
        .map(|d| char::from_u32(0x2080 + d.to_digit(10).unwrap()).unwrap())
        .collect()
}

#[derive(Default, Clone, PartialEq)]
struct State {
    workspaces: Vec<serde_json::Value>,
    mode: String,
    title: String,
    /// The scratchpad windows: "app: title" each.
    scratch: Vec<String>,
    /// The focused window is one of them.
    scratch_focused: bool,
}

pub async fn run(src: Source, hub: Hub) {
    loop {
        if let Err(e) = follow(&src, &hub).await {
            eprintln!("{}: {e:#}; reconnecting in 2s", src.name);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

async fn follow(src: &Source, hub: &Hub) -> Result<()> {
    let mut conn = Connection::new().await.context("connecting to sway")?;
    let mut events = Connection::new()
        .await?
        .subscribe([EventType::Workspace, EventType::Window, EventType::Mode])
        .await
        .context("subscribing")?;

    let mut s = State::default();
    read_workspaces(&mut conn, &mut s).await?;
    s.mode = conn.get_binding_state().await.unwrap_or_else(|_| "default".into());
    read_tree(&mut conn, &mut s).await?;
    let mut last = State::default();
    emit(src, hub, &s, &mut last);

    loop {
        let ev = match events.next().await {
            Some(Ok(ev)) => ev,
            Some(Err(e)) => return Err(e.into()),
            None => anyhow::bail!("event stream ended"),
        };
        apply(&mut conn, &mut s, ev).await?;
        // a switch is a handful of events a few hundred microseconds apart
        tokio::time::sleep(Duration::from_millis(5)).await;
        while let Ok(Some(Ok(ev))) = tokio::time::timeout(Duration::from_millis(1), events.next()).await {
            apply(&mut conn, &mut s, ev).await?;
        }
        emit(src, hub, &s, &mut last);
    }
}

async fn apply(conn: &mut Connection, s: &mut State, ev: Event) -> Result<()> {
    match ev {
        Event::Workspace(w) => {
            read_workspaces(conn, s).await?;
            if w.change == WorkspaceChange::Focus {
                // an empty workspace brings no focus event: no title until
                // a window says otherwise
                s.title.clear();
                s.scratch_focused = false;
            }
        }
        Event::Window(w) => {
            let c = &w.container;
            match w.change {
                WindowChange::Focus => {
                    s.title = c.name.clone().unwrap_or_default();
                    s.scratch_focused = in_scratchpad(c);
                }
                WindowChange::Title => {
                    if c.focused {
                        s.title = c.name.clone().unwrap_or_default();
                    }
                    if in_scratchpad(c) {
                        read_tree(conn, s).await?;
                    }
                }
                WindowChange::Close => {
                    if c.focused {
                        s.title.clear();
                        s.scratch_focused = false;
                    }
                    if in_scratchpad(c) {
                        read_tree(conn, s).await?;
                    }
                }
                // in or out of the scratchpad, or a new window on a
                // workspace whose urgency changes
                WindowChange::Move | WindowChange::Floating => read_tree(conn, s).await?,
                WindowChange::Urgent => read_workspaces(conn, s).await?,
                _ => {}
            }
        }
        Event::Mode(m) => s.mode = m.change,
        _ => {}
    }
    Ok(())
}

fn in_scratchpad(n: &Node) -> bool {
    matches!(n.scratchpad_state, Some(ScratchpadState::Fresh) | Some(ScratchpadState::Changed))
}

async fn read_workspaces(conn: &mut Connection, s: &mut State) -> Result<()> {
    s.workspaces = conn
        .get_workspaces()
        .await?
        .iter()
        .map(|w| {
            json!({
                "name": w.name, "num": w.num, "focused": w.focused, "visible": w.visible,
                "urgent": w.urgent, "output": w.output,
            })
        })
        .collect();
    Ok(())
}

/// The tree, for the scratchpad windows (and the title, in case the
/// events left it stale).
async fn read_tree(conn: &mut Connection, s: &mut State) -> Result<()> {
    let tree = conn.get_tree().await?;
    let mut title = None;
    let mut scratch = Vec::new();
    let mut focused = false;
    walk(&tree, &mut |n| {
        if n.focused && n.node_type != NodeType::Workspace && title.is_none() {
            title = Some(n.name.clone().unwrap_or_default());
        }
        if in_scratchpad(n) {
            let app = n
                .app_id
                .clone()
                .or_else(|| n.window_properties.as_ref().and_then(|w| w.class.clone()))
                .unwrap_or_else(|| "?".into());
            scratch.push(format!("{app}: {}", n.name.clone().unwrap_or_default()));
            focused |= n.focused;
        }
    });
    s.title = title.unwrap_or_default();
    s.scratch = scratch;
    s.scratch_focused = focused;
    Ok(())
}

fn walk(n: &Node, f: &mut dyn FnMut(&Node)) {
    f(n);
    for c in n.nodes.iter().chain(n.floating_nodes.iter()) {
        walk(c, f);
    }
}

fn emit(src: &Source, hub: &Hub, s: &State, last: &mut State) {
    if s.workspaces != last.workspaces || s.mode != last.mode || s.title != last.title {
        hub.emit(&src.name, json!({ "workspaces": s.workspaces, "mode": s.mode, "title": s.title }));
    }
    if s.scratch != last.scratch || s.scratch_focused != last.scratch_focused {
        hub.emit(
            "scratchpad",
            json!({
                "text": format!("{SCRATCH_ICON}{}", subscript(s.scratch.len())),
                "info": s.scratch.join("\n"),
                "class": if s.scratch_focused { "focused" } else { "" },
                "count": s.scratch.len(),
            }),
        );
    }
    *last = s.clone();
}
