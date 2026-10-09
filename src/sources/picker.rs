//! The picker's data side: what the launcher lists (the desktop entries)
//! and does (runs the chosen one), what the window switcher lists (the
//! windows in sway's tree) and does (focuses it), and the dmenu mode,
//! where the lines come from a client's stdin and the choice goes back to
//! it. The window itself is ui/picker.rs.

use super::Hub;
use crate::runner;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::sync::oneshot;

/// One line of a picker.
#[derive(Debug, Clone)]
pub struct Item {
    pub label: String,
    /// Smaller text after the label (a window's title, an app's comment).
    pub detail: String,
    /// An icon name, or a file path.
    pub icon: String,
    /// What the frecency is kept by.
    pub key: String,
    /// More words the match looks at, weighed like the detail (generic
    /// name, keywords).
    pub extra: String,
    /// Words the match looks at last (an app's comment).
    pub comment: String,
    /// A window of an app above it: shown under it, matched with it.
    pub parent: Option<usize>,
}

/// What a line of the launcher stands for.
#[derive(Debug, Clone, Copy)]
pub enum Target {
    App(usize),
    Window(usize),
}

/// What the picker answers.
#[derive(Debug)]
pub enum Choice {
    Picked(usize),
    Cancelled,
    /// Ctrl+Tab: the other list (launcher <-> windows).
    Switch,
}

#[derive(Debug)]
pub struct PickRequest {
    /// The frecency namespace: "launch", "windows", or the dmenu name.
    pub name: String,
    pub prompt: String,
    /// Text already in the field when it opens.
    pub query: String,
    pub items: Vec<Item>,
    pub reply: oneshot::Sender<Choice>,
}

// ---- dmenu --------------------------------------------------------------

/// dmenu lines: "text", or with an icon "text\0icon\x1fNAME".
pub fn dmenu_items(lines: &[String]) -> Vec<Item> {
    lines
        .iter()
        .map(|l| {
            let (text, rest) = l.split_once('\0').unwrap_or((l, ""));
            let icon = rest
                .split('\x1f')
                .nth(1)
                .map(|i| i.split(',').next().unwrap_or("").to_string())
                .unwrap_or_default();
            Item {
                label: text.to_string(),
                detail: String::new(),
                icon,
                key: text.to_string(),
                extra: String::new(),
                comment: String::new(),
                parent: None,
            }
        })
        .collect()
}

// ---- the launcher -------------------------------------------------------

#[derive(Debug, Clone)]
pub struct App {
    pub id: String,
    pub name: String,
    pub generic: String,
    pub comment: String,
    pub keywords: String,
    pub icon: String,
    pub exec: String,
    pub terminal: bool,
    pub path: Option<String>,
    /// The entry says the app has one main window and opens no other
    /// (SingleMainWindow, or GNOME's older X-GNOME-SingleWindow).
    pub single_window: bool,
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut dirs = vec![std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"))];
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(sys.split(':').filter(|d| !d.is_empty()).map(PathBuf::from));
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share"));
    dirs.push(PathBuf::from(&home).join(".local/share/flatpak/exports/share"));
    dirs.into_iter().map(|d| d.join("applications")).collect()
}

/// The desktop entries, by id, the first directory in XDG order winning.
pub fn desktop_entries() -> Vec<App> {
    let mut seen: HashMap<String, App> = HashMap::new();
    for dir in data_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let id = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
            if seen.contains_key(&id) {
                continue;
            }
            if let Some(app) = parse_entry(&path, &id) {
                seen.insert(id, app);
            }
        }
    }
    let mut apps: Vec<App> = seen.into_values().collect();
    apps.sort_by_key(|a| a.name.to_lowercase());
    apps
}

fn parse_entry(path: &Path, id: &str) -> Option<App> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut in_main = false;
    let mut f: HashMap<&str, &str> = HashMap::new();
    let lang = std::env::var("LANG").unwrap_or_default();
    let lang = lang.split(['.', '_']).next().unwrap_or("").to_string();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main = line == "[Desktop Entry]";
            continue;
        }
        if !in_main {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { continue };
        f.insert(k.trim(), v.trim());
    }
    if f.get("Type").copied().unwrap_or("Application") != "Application" {
        return None;
    }
    if f.get("NoDisplay") == Some(&"true") || f.get("Hidden") == Some(&"true") {
        return None;
    }
    let exec = f.get("Exec")?.to_string();
    // a translated name when there is one for the locale
    let localized = |key: &str| {
        let lk = format!("{key}[{lang}]");
        f.get(lk.as_str()).or_else(|| f.get(key)).copied().unwrap_or("").to_string()
    };
    Some(App {
        id: id.to_string(),
        name: localized("Name"),
        generic: localized("GenericName"),
        comment: localized("Comment"),
        keywords: localized("Keywords").replace(';', " "),
        icon: f.get("Icon").copied().unwrap_or("").to_string(),
        exec,
        terminal: f.get("Terminal") == Some(&"true"),
        path: f.get("Path").map(|p| p.to_string()),
        single_window: f.get("SingleMainWindow") == Some(&"true") || f.get("X-GNOME-SingleWindow") == Some(&"true"),
    })
}

/// Run an entry: its Exec line without the field codes, in a terminal
/// when it asks for one.
pub fn launch(app: &App, hub: &Hub) {
    let mut exec = String::new();
    let mut chars = app.exec.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '%' {
            // %% is a percent; %f %F %u %U %i %c %k and the rest: nothing to pass
            if chars.next() == Some('%') {
                exec.push('%');
            }
        } else {
            exec.push(c);
        }
    }
    let exec = exec.trim().to_string();
    let cmd = if app.terminal {
        format!("{} {exec}", hub.cfg.picker.terminal)
    } else {
        exec
    };
    let cmd = match &app.path {
        Some(p) if !p.is_empty() => format!("cd {} && {cmd}", shell_quote(p)),
        _ => cmd,
    };
    runner::detached(&cmd, hub.scripts());
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

// ---- the window switcher ------------------------------------------------

#[derive(Debug, Clone)]
pub struct Window {
    pub id: i64,
    pub app: String,
    pub workspace: String,
    pub title: String,
    pub scratchpad: bool,
}

pub async fn windows() -> Result<Vec<Window>> {
    use swayipc_async::{Node, NodeType, ScratchpadState};
    let mut conn = crate::swaysock::connect().await?;
    let tree = conn.get_tree().await?;
    let mut out = Vec::new();
    // `scratch`: the node or an ancestor is in the scratchpad (a window
    // inside a tabbed container sent there carries no state of its own)
    fn walk(n: &Node, ws: &str, scratch: bool, out: &mut Vec<Window>) {
        let scratch = scratch || matches!(n.scratchpad_state, Some(ScratchpadState::Fresh) | Some(ScratchpadState::Changed));
        let ws = if n.node_type == NodeType::Workspace {
            let name = n.name.clone().unwrap_or_default();
            if name == "__i3_scratch" {
                "scratch".to_string()
            } else {
                name
            }
        } else {
            ws.to_string()
        };
        if n.pid.is_some() {
            out.push(Window {
                id: n.id,
                app: n
                    .app_id
                    .clone()
                    .or_else(|| n.window_properties.as_ref().and_then(|w| w.class.clone()))
                    .unwrap_or_else(|| "?".into()),
                workspace: ws.clone(),
                title: n.name.clone().unwrap_or_default(),
                scratchpad: scratch,
            });
        }
        for c in n.nodes.iter().chain(n.floating_nodes.iter()) {
            walk(c, &ws, scratch, out);
        }
    }
    walk(&tree, "", false, &mut out);
    Ok(out)
}

/// The windows as items: the app's name and icon from its desktop entry
/// when there is one (by id, else by StartupWMClass), else the app id.
pub fn window_items(windows: &[Window], apps: &[App]) -> Vec<Item> {
    let by_id: HashMap<String, &App> = apps.iter().map(|a| (a.id.to_lowercase(), a)).collect();
    windows
        .iter()
        .map(|w| {
            let app = by_id.get(&w.app.to_lowercase()).copied();
            let (name, icon) = match app {
                Some(a) => (a.name.clone(), a.icon.clone()),
                None => (w.app.clone(), w.app.to_lowercase()),
            };
            Item {
                label: format!("[{}] {name}", w.workspace),
                detail: w.title.clone(),
                icon: if icon.is_empty() { "application-x-executable".into() } else { icon },
                key: w.app.clone(),
                extra: w.title.clone(),
                comment: String::new(),
                parent: None,
            }
        })
        .collect()
}

/// The launcher's list: every app, and under each one its open windows,
/// which focus directly; a window whose app has no entry stands alone.
/// An app that declares a single main window and has it open shows only
/// that window, in its own place: a second instance is not a choice.
pub fn launch_items(apps: &[App], windows: &[Window]) -> (Vec<Item>, Vec<Target>) {
    let by_id: HashMap<String, usize> = apps.iter().enumerate().map(|(i, a)| (a.id.to_lowercase(), i)).collect();
    let mut items = Vec::new();
    let mut targets = Vec::new();
    let mut placed = vec![false; windows.len()];
    for (ai, a) in apps.iter().enumerate() {
        let open: Vec<usize> = windows
            .iter()
            .enumerate()
            .filter(|(_, w)| by_id.get(&w.app.to_lowercase()) == Some(&ai))
            .map(|(i, _)| i)
            .collect();
        if a.single_window {
            if let Some(&wi) = open.first() {
                let w = &windows[wi];
                placed[wi] = true;
                items.push(Item {
                    label: format!("{}  [{}] {}", a.name, w.workspace, w.title),
                    detail: if a.generic.is_empty() {
                        a.comment.clone()
                    } else {
                        a.generic.clone()
                    },
                    icon: a.icon.clone(),
                    key: a.id.clone(),
                    extra: format!("{} {}", a.generic, a.keywords),
                    comment: a.comment.clone(),
                    parent: None,
                });
                targets.push(Target::Window(wi));
                continue;
            }
        }
        let parent = items.len();
        items.push(Item {
            label: a.name.clone(),
            detail: if a.generic.is_empty() {
                a.comment.clone()
            } else {
                a.generic.clone()
            },
            icon: a.icon.clone(),
            key: a.id.clone(),
            extra: format!("{} {}", a.generic, a.keywords),
            comment: a.comment.clone(),
            parent: None,
        });
        targets.push(Target::App(ai));
        for wi in open {
            let w = &windows[wi];
            placed[wi] = true;
            items.push(Item {
                label: format!("[{}] {}", w.workspace, w.title),
                detail: String::new(),
                icon: a.icon.clone(),
                key: a.id.clone(),
                extra: String::new(),
                comment: String::new(),
                parent: Some(parent),
            });
            targets.push(Target::Window(wi));
        }
    }
    for (wi, w) in windows.iter().enumerate() {
        if placed[wi] {
            continue;
        }
        items.push(Item {
            label: format!("[{}] {} — {}", w.workspace, w.app, w.title),
            detail: String::new(),
            icon: w.app.to_lowercase(),
            key: w.app.clone(),
            extra: String::new(),
            comment: String::new(),
            parent: None,
        });
        targets.push(Target::Window(wi));
    }
    (items, targets)
}

/// Focus a window. sway's focus brings a scratchpad window up too; the
/// picker's `scratchpad` command, when set, does it instead (a script
/// that places it its own way).
pub async fn focus(w: &Window, hub: &Hub) -> Result<()> {
    let show = &hub.cfg.picker.scratchpad;
    if w.scratchpad && !show.is_empty() {
        runner::detached(&show.replace("{id}", &w.id.to_string()), hub.scripts());
        return Ok(());
    }
    let mut conn = crate::swaysock::connect().await?;
    conn.run_command(format!("[con_id={}] focus", w.id)).await?;
    Ok(())
}
