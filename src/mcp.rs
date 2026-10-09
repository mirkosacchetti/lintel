//! `lintel mcp`: a Model Context Protocol server on stdin/stdout, for an
//! assistant that debugs the bar and the desktop around it. It reads what
//! the bar shows (every module rendered as on screen, every variable, the
//! notifications), sway's state (the windows with their processes, the
//! workspaces, the outputs, any IPC query), takes screenshots, runs sway
//! commands and reads the journal.
//!
//! JSON-RPC 2.0, one message per line, no async: each call is a socket
//! request to the bar or a short child process (swaymsg, grim, journalctl).
//! The bar need not be running: the sway and journal tools work without it.

use crate::config::{Config, Module};
use crate::ipc;
use crate::template::{walk_path, Template};
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::Command;

const PROTOCOL: &str = "2025-06-18";

const INSTRUCTIONS: &str = "\
lintel's own view of this sway desktop. bar_state shows every module as \
it is on screen (text, card info, class, mouse actions) and the raw \
variables behind them; notifications lists what was shown; sway_windows \
lists every window with its workspace, output, geometry and process; \
sway_get answers any sway IPC query; sway_command runs sway commands \
(move, focus, workspace, resize, output ...); screenshot captures an \
output, a window or a region; logs reads the journal (lintel's unit by \
default, any unit or syslog tag otherwise). Where things live: the bar's \
config ~/.config/lintel/lintel.toml and style.css (its [bar] scripts \
names the directory of the scripts its modules call, $S in the \
commands), the user unit lintel (systemctl --user restart lintel), \
sway's config ~/.config/sway/config. Notifications that pop up at login usually come \
from a script started by sway's exec lines: the logs tool with the \
script's syslog tag, or the notifications tool, says which.";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                reply(
                    &mut out,
                    &json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": e.to_string()}}),
                )?;
                continue;
            }
        };
        // a notification (no id) wants no answer
        let Some(id) = msg.get("id").cloned() else { continue };
        let method = msg["method"].as_str().unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let answer = match method {
            "initialize" => Ok(json!({
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "lintel", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS,
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tools()})),
            "tools/call" => Ok(call(params["name"].as_str().unwrap_or(""), &params["arguments"])),
            _ => Err(json!({"code": -32601, "message": format!("unknown method {method}")})),
        };
        let msg = match answer {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        };
        reply(&mut out, &msg)?;
    }
    Ok(())
}

fn reply(out: &mut impl Write, msg: &Value) -> Result<()> {
    writeln!(out, "{msg}")?;
    out.flush()?;
    Ok(())
}

fn tool(name: &str, description: &str, properties: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties},
    })
}

fn tools() -> Vec<Value> {
    vec![
        tool(
            "bar_state",
            "Every module of the bar as on screen: its text, the card's info, its class, whether it is hidden, \
             its mouse actions and hints; list modules (workspaces, countdowns) with their items. Then the raw \
             variables the modules render (sway, clock, battery, network, audio, mpris, display, tray ...).",
            json!({
                "modules": {"type": "array", "items": {"type": "string"}, "description": "Only these modules (by name); all when absent."},
                "variables": {"type": "boolean", "description": "Include the raw variables (default true)."},
            }),
        ),
        tool(
            "bar_command",
            "Send a request to the running bar, as `lintel ...` does: `refresh NAME`, `get NAME`, `update NAME VALUE`, \
             `pomodoro start|stop|pause|resume|toggle|skip|status`, `timer 7m LABEL`, `timer stop [LABEL]`, \
             `timer status`, `notifications close|close-all|pop|clear|toggle|status`, `cancel`. The interactive \
             ones (pick, launch, windows, notifications history) wait for the user and are refused.",
            json!({"command": {"type": "string", "description": "The request line, e.g. \"refresh brightness\"."}}),
        ),
        tool(
            "notifications",
            "The notifications lintel's daemon received: those on screen and the history, newest first, with \
             app, summary, body, time and urgency.",
            json!({}),
        ),
        tool(
            "sway_windows",
            "Every window: con id, app id (or X11 class), title, workspace, output, focused, visible, floating, \
             scratchpad, fullscreen, urgent, geometry, marks, pid and the process's command line with its \
             children's (what runs inside a terminal). Then the workspaces and outputs in short.",
            json!({}),
        ),
        tool(
            "sway_get",
            "A sway IPC query, raw JSON: tree, workspaces, outputs, inputs, seats, marks, bar_config, version, \
             binding_modes, binding_state, config (the loaded config text).",
            json!({"type": {"type": "string", "enum": ["tree", "workspaces", "outputs", "inputs", "seats", "marks", "bar_config", "version", "binding_modes", "binding_state", "config"]}}),
        ),
        tool(
            "sway_command",
            "Run sway commands, as swaymsg does: `[con_id=12] move to workspace 3`, `workspace 2`, \
             `[app_id=foo] focus`, `output DP-1 mode 3840x2160@60Hz`, `reload`, several separated by `;` or `,`. \
             Answers each command's success or error.",
            json!({"command": {"type": "string"}}),
        ),
        tool(
            "screenshot",
            "Capture the screen (grim) and return the image, also saved to a file whose path is given. \
             Without arguments the focused output. A window must be visible to be captured.",
            json!({
                "output": {"type": "string", "description": "An output name (DP-1, eDP-1), or \"all\"."},
                "window": {"type": "integer", "description": "A window's con id (from sway_windows)."},
                "region": {"type": "string", "description": "\"X,Y WxH\" in layout coordinates."},
                "scale": {"type": "number", "description": "Image scale, default 0.5; 1 for full detail."},
            }),
        ),
        tool(
            "logs",
            "Read the journal. Default: the lintel unit's last 100 lines. `identifier` filters by syslog tag \
             (what `logger -t TAG` writes, e.g. a script's), `unit` by unit, `all` reads the whole user journal; \
             `system` reads the system journal instead (kernel, amdgpu, logind ...).",
            json!({
                "unit": {"type": "string"},
                "identifier": {"type": "string"},
                "all": {"type": "boolean"},
                "system": {"type": "boolean"},
                "kernel": {"type": "boolean", "description": "Kernel messages only (implies system)."},
                "lines": {"type": "integer", "description": "Default 100."},
                "since": {"type": "string", "description": "As journalctl takes it: \"10 min ago\", \"today\", \"2026-10-09 09:30\"."},
                "grep": {"type": "string", "description": "A pattern the messages must match (case-insensitive)."},
            }),
        ),
    ]
}

/// A tool's result: text, or an image and its text.
fn call(name: &str, args: &Value) -> Value {
    let result = match name {
        "bar_state" => bar_state(args).map(text),
        "bar_command" => bar_command(args).map(text),
        "notifications" => ipc::send("notifications list").map(|r| text(pretty(&r))),
        "sway_windows" => sway_windows().map(text),
        "sway_get" => sway_get(args).map(text),
        "sway_command" => sway_command(args).map(text),
        "screenshot" => screenshot(args),
        "logs" => logs(args).map(text),
        _ => Err(anyhow::anyhow!("unknown tool {name}")),
    };
    result.unwrap_or_else(|e| json!({"content": [{"type": "text", "text": format!("{e:#}")}], "isError": true}))
}

fn text(s: String) -> Value {
    json!({"content": [{"type": "text", "text": s}]})
}

fn pretty(raw: &str) -> String {
    serde_json::from_str::<Value>(raw)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| raw.to_string())
}

// ---- the bar ------------------------------------------------------------

fn bar_state(args: &Value) -> Result<String> {
    let cfg = Config::load(None)?;
    let vars: Value = serde_json::from_str(&ipc::send("state")?).context("the bar's state")?;
    let only: Option<Vec<&str>> = args["modules"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect());
    let lookup = |name: &str| vars.get(name).cloned();
    let modules: Vec<Value> = cfg
        .modules
        .iter()
        .filter(|m| only.as_ref().is_none_or(|o| o.contains(&m.name.as_str())))
        .map(|m| render_module(m, &vars, &lookup))
        .collect();
    let mut state = json!({"modules": modules});
    if args["variables"].as_bool().unwrap_or(true) {
        state["variables"] = vars;
    }
    Ok(serde_json::to_string_pretty(&state)?)
}

fn render_module(m: &Module, vars: &Value, lookup: &dyn Fn(&str) -> Option<Value>) -> Value {
    let render = |t: &str| Template::parse(t).render(lookup);
    let mut out = json!({"name": m.name, "side": m.side, "kind": m.kind});
    if !m.title.is_empty() {
        out["title"] = json!(m.title);
    }
    match m.kind.as_str() {
        "list" | "workspaces" => {
            let (var, path) = m.items.split_once('.').unwrap_or((m.items.as_str(), ""));
            let items = vars.get(var).and_then(|v| walk_path(v, path)).and_then(Value::as_array);
            let (text_t, class_t) = (Template::parse(&m.item_text), Template::parse(&m.item_class));
            out["items"] = items
                .map(|a| {
                    a.iter()
                        .map(|i| json!({"text": text_t.render_item(i), "class": class_t.render_item(i)}))
                        .collect()
                })
                .unwrap_or_else(|| json!([]));
        }
        "tray" => out["items"] = vars.get("tray").cloned().unwrap_or(Value::Null),
        _ => {
            let text = render(&m.text);
            out["hidden"] = json!(text.is_empty() || m.hide_when.as_deref() == Some(text.as_str()));
            out["text"] = json!(text);
            out["info"] = json!(render(&m.info));
            out["class"] = json!(render(&m.class));
        }
    }
    let actions: serde_json::Map<String, Value> = [
        ("click", &m.click),
        ("middle", &m.middle),
        ("right", &m.right),
        ("scroll_up", &m.scroll_up),
        ("scroll_down", &m.scroll_down),
    ]
    .into_iter()
    .filter(|(_, c)| !c.is_empty())
    .map(|(k, c)| (k.to_string(), json!(c)))
    .collect();
    if !actions.is_empty() {
        out["actions"] = Value::Object(actions);
    }
    if !m.hints.is_empty() {
        out["hints"] = json!(m.hints);
    }
    out
}

fn bar_command(args: &Value) -> Result<String> {
    let line = args["command"].as_str().unwrap_or("").trim();
    let first = line.split_whitespace().next().unwrap_or("");
    if line.is_empty() {
        bail!("no command");
    }
    if matches!(first, "pick" | "launch" | "windows" | "quit") || line == "notifications history" {
        bail!("{first}: interactive or fatal, not from here");
    }
    ipc::send(line).map(|r| pretty(&r))
}

// ---- sway ---------------------------------------------------------------

fn swaymsg(args: &[&str]) -> Result<Value> {
    let mut c = Command::new("swaymsg");
    if let Some(sock) = crate::swaysock::path() {
        c.arg("-s").arg(sock);
    }
    let out = c.arg("-r").args(args).output().context("running swaymsg")?;
    let v: Value =
        serde_json::from_slice(&out.stdout).with_context(|| format!("swaymsg: {}", String::from_utf8_lossy(&out.stderr).trim()))?;
    Ok(v)
}

fn sway_get(args: &Value) -> Result<String> {
    let kind = args["type"].as_str().unwrap_or("tree");
    let v = swaymsg(&["-t", &format!("get_{kind}")])?;
    // the config comes as {"config": "..."}: the text itself reads better
    if let Some(c) = v.get("config").and_then(Value::as_str) {
        return Ok(c.to_string());
    }
    Ok(serde_json::to_string_pretty(&v)?)
}

fn sway_command(args: &Value) -> Result<String> {
    let cmd = args["command"].as_str().unwrap_or("").trim();
    if cmd.is_empty() {
        bail!("no command");
    }
    let v = swaymsg(&["--", cmd])?;
    Ok(serde_json::to_string_pretty(&v)?)
}

/// A process's command line, from /proc.
fn cmdline(pid: u64) -> String {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| {
            b.split(|&c| c == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

/// The process's children, and theirs: a terminal's shell and what it runs.
fn descendants(pid: u64, depth: u32, out: &mut Vec<Value>) {
    if depth == 0 {
        return;
    }
    let Ok(list) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
        return;
    };
    for child in list.split_whitespace().filter_map(|c| c.parse::<u64>().ok()) {
        out.push(json!({"pid": child, "cmdline": cmdline(child)}));
        descendants(child, depth - 1, out);
    }
}

fn sway_windows() -> Result<String> {
    let tree = swaymsg(&["-t", "get_tree"])?;
    let mut windows = Vec::new();
    walk(&tree, "", "", &mut windows);
    let workspaces: Vec<Value> = swaymsg(&["-t", "get_workspaces"])?
        .as_array()
        .into_iter()
        .flatten()
        .map(|w| json!({"name": w["name"], "output": w["output"], "focused": w["focused"], "visible": w["visible"], "urgent": w["urgent"]}))
        .collect();
    let outputs: Vec<Value> = swaymsg(&["-t", "get_outputs"])?
        .as_array()
        .into_iter()
        .flatten()
        .map(|o| {
            let m = &o["current_mode"];
            json!({
                "name": o["name"], "make": o["make"], "model": o["model"], "active": o["active"],
                "focused": o["focused"], "scale": o["scale"], "transform": o["transform"], "rect": o["rect"],
                "mode": m.is_object().then(|| {
                    format!("{}x{}@{:.3}Hz", m["width"], m["height"], m["refresh"].as_f64().unwrap_or(0.0) / 1000.0)
                }),
                "current_workspace": o["current_workspace"],
            })
        })
        .collect();
    Ok(serde_json::to_string_pretty(
        &json!({"windows": windows, "workspaces": workspaces, "outputs": outputs}),
    )?)
}

fn walk(node: &Value, output: &str, workspace: &str, out: &mut Vec<Value>) {
    let kind = node["type"].as_str().unwrap_or("");
    let name = node["name"].as_str().unwrap_or("");
    let output = if kind == "output" { name } else { output };
    let workspace = if kind == "workspace" { name } else { workspace };
    if let Some(pid) = node["pid"].as_u64() {
        let mut children = Vec::new();
        descendants(pid, 3, &mut children);
        let scratchpad = node["scratchpad_state"].as_str().is_some_and(|s| s != "none");
        out.push(json!({
            "id": node["id"],
            "app_id": node["app_id"].as_str().or_else(|| node["window_properties"]["class"].as_str()),
            "shell": node["shell"],
            "title": node["name"],
            "workspace": if workspace == "__i3_scratch" { "(scratchpad, hidden)" } else { workspace },
            "output": output,
            "focused": node["focused"],
            "visible": node["visible"],
            "floating": kind == "floating_con",
            "scratchpad": scratchpad,
            "fullscreen": node["fullscreen_mode"].as_u64().unwrap_or(0) != 0,
            "urgent": node["urgent"],
            "rect": node["rect"],
            "marks": node["marks"],
            "inhibit_idle": node["inhibit_idle"],
            "pid": pid,
            "cmdline": cmdline(pid),
            "children": children,
        }));
    }
    for key in ["nodes", "floating_nodes"] {
        for c in node[key].as_array().into_iter().flatten() {
            walk(c, output, workspace, out);
        }
    }
}

// ---- screenshots ---------------------------------------------------------

fn screenshot(args: &Value) -> Result<Value> {
    let scale = args["scale"].as_f64().unwrap_or(0.5).clamp(0.1, 2.0);
    let mut grim = Command::new("grim");
    grim.args(["-t", "jpeg", "-q", "85", "-s", &scale.to_string()]);
    let what = if let Some(id) = args["window"].as_u64() {
        let tree = swaymsg(&["-t", "get_tree"])?;
        let node = find_con(&tree, id).with_context(|| format!("no window with con id {id}"))?;
        if node["visible"].as_bool() == Some(false) {
            bail!("window {id} is not visible: focus it first (sway_command \"[con_id={id}] focus\")");
        }
        let r = &node["rect"];
        grim.args(["-g", &format!("{},{} {}x{}", r["x"], r["y"], r["width"], r["height"])]);
        format!("window {id}")
    } else if let Some(region) = args["region"].as_str() {
        grim.args(["-g", region]);
        format!("region {region}")
    } else {
        let output = match args["output"].as_str() {
            Some(o) => o.to_string(),
            None => swaymsg(&["-t", "get_outputs"])?
                .as_array()
                .into_iter()
                .flatten()
                .find(|o| o["focused"].as_bool() == Some(true))
                .and_then(|o| o["name"].as_str().map(String::from))
                .context("no focused output")?,
        };
        if output != "all" {
            grim.args(["-o", &output]);
        }
        format!("output {output}")
    };
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("lintel-shots");
    std::fs::create_dir_all(&dir)?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S%.3f");
    let path = dir.join(format!("{stamp}.jpg"));
    let out = grim.arg(&path).output().context("running grim")?;
    if !out.status.success() {
        bail!("grim: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let data = std::fs::read(&path)?;
    Ok(json!({"content": [
        {"type": "image", "data": base64(&data), "mimeType": "image/jpeg"},
        {"type": "text", "text": format!("{what}, scale {scale}: {}", path.display())},
    ]}))
}

fn find_con(node: &Value, id: u64) -> Option<&Value> {
    if node["id"].as_u64() == Some(id) {
        return Some(node);
    }
    ["nodes", "floating_nodes"]
        .iter()
        .flat_map(|k| node[*k].as_array().into_iter().flatten())
        .find_map(|c| find_con(c, id))
}

fn base64(data: &[u8]) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

// ---- the journal ----------------------------------------------------------

fn logs(args: &Value) -> Result<String> {
    let kernel = args["kernel"].as_bool().unwrap_or(false);
    let system = kernel || args["system"].as_bool().unwrap_or(false);
    let lines = args["lines"].as_u64().unwrap_or(100).clamp(1, 5000);
    let mut j = Command::new("journalctl");
    j.args(["--no-pager", "-o", "short-iso", "-n", &lines.to_string()]);
    if !system {
        j.arg("--user");
    }
    if kernel {
        j.arg("-k");
    } else if let Some(t) = args["identifier"].as_str() {
        j.args(["-t", t]);
    } else if let Some(u) = args["unit"].as_str() {
        j.args(["-u", u]);
    } else if !args["all"].as_bool().unwrap_or(false) && !system {
        j.args(["-u", "lintel"]);
    }
    if let Some(s) = args["since"].as_str() {
        j.args(["--since", s]);
    }
    if let Some(g) = args["grep"].as_str() {
        j.args(["-i", "-g", g]);
    }
    let out = j.output().context("running journalctl")?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    if !out.status.success() && text.trim().is_empty() {
        bail!("journalctl: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
