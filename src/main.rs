//! lintel: an event-driven bar for sway, with a launcher, a notification
//! daemon and a pomodoro inside. `lintel` runs the bar; the other
//! subcommands talk to it over its socket.

mod config;
mod ipc;
mod locale;
mod mcp;
mod runner;
mod sources;
mod template;
mod ui;

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;

const USAGE: &str = "\
lintel — an event-driven bar, with a pomodoro

  lintel [--config FILE]        run the bar
  lintel refresh NAME           take source NAME's snapshot again
  lintel update NAME VALUE      set variable NAME (VALUE: JSON or text)
  lintel get NAME               print variable NAME
  lintel state                  every variable, as one JSON object
  lintel pomodoro CMD           start | stop | pause | resume | toggle | skip | status
  lintel timer DURATION [LABEL] a countdown (7m, 90s, 1h30m); the label is its key
  lintel timer stop [LABEL]     end one countdown, or all; `timer status` lists them
  lintel pick [NAME] [PROMPT]   choose one of stdin's lines (dmenu style); prints it,
                                 or with --index its number; NAME keeps a frecency of its own
  lintel launch [TEXT]          the application launcher, TEXT already typed (Ctrl+Tab: the windows)
  lintel windows [TEXT]         the window switcher (Ctrl+Tab: the launcher)
  lintel cancel                 close the picker
  lintel notifications CMD      close | close-all | action | pop | history | clear | toggle | status | list
  lintel mcp                    an MCP server on stdin/stdout: the bar, sway, screenshots, logs
  lintel quit
";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None => daemon(None),
        Some("--config") => daemon(args.get(1).map(PathBuf::from)),
        Some("pick") => {
            let index = args.iter().any(|a| a == "--index");
            let rest: Vec<&String> = args.iter().skip(1).filter(|a| !a.starts_with("--")).collect();
            let name = rest.first().map(|s| s.as_str()).unwrap_or("dmenu");
            let prompt = rest.get(1).map(|s| s.as_str()).unwrap_or("");
            let lines: Vec<String> = std::io::stdin().lines().map_while(Result::ok).collect();
            let reply = ipc::send_lines(&format!("pick {name} {prompt}"), &lines)?;
            let Some((i, text)) = reply.split_once('\t') else {
                std::process::exit(1)
            };
            println!("{}", if index { i } else { text });
            Ok(())
        }
        Some("mcp") => mcp::serve(),
        Some(
            "refresh" | "update" | "get" | "state" | "pomodoro" | "timer" | "launch" | "windows" | "cancel" | "notifications" | "quit",
        ) => {
            let reply = ipc::send(&args.join(" "))?;
            if !reply.is_empty() && reply != "ok" {
                println!("{reply}");
            }
            if reply.starts_with("error") {
                std::process::exit(1);
            }
            Ok(())
        }
        Some("help" | "-h" | "--help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some(other) => {
            eprintln!("unknown command {other:?}\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}

fn daemon(config: Option<PathBuf>) -> Result<()> {
    let cfg = Arc::new(config::Config::load(config)?);
    let (out_tx, out_rx) = async_channel::unbounded();
    let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();

    // the sources on a tokio runtime of their own; GTK keeps the main thread
    let (cfg2, req_tx2) = (cfg.clone(), req_tx.clone());
    std::thread::Builder::new().name("sources".into()).spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");
        rt.block_on(async move {
            tokio::spawn(async move {
                if let Err(e) = ipc::serve(req_tx2).await {
                    eprintln!("ipc: {e:#}");
                }
            });
            sources::run(cfg2, out_tx, req_rx).await;
        });
        // dropping the runtime cancels the sources: their listeners'
        // process groups are terminated, the update channel closes and
        // the GTK loop ends
        drop(rt);
    })?;

    ui::run(cfg, out_rx, req_tx)
}
