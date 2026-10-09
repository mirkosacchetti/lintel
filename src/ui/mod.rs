//! The GTK side: one bar per monitor, kept in step with hotplugs through
//! the display's monitor list, the picker, the notifications, and the
//! store the sources' updates land in. GTK 4: the renderer speaks the
//! fractional-scale protocol, so a 1.5 scale is sharp.

pub mod bar;
pub mod calendar;
pub mod notify;
pub mod picker;
pub mod store;
pub mod tray;

use crate::config::{config_dir, Config};
use crate::ipc::Request;
use crate::sources::Message;
use anyhow::{Context, Result};
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use store::Store;
use tokio::sync::mpsc::UnboundedSender;

const DEFAULT_CSS: &str = include_str!("../../config/style.css");

pub fn run(cfg: Arc<Config>, updates: async_channel::Receiver<Message>, reqs: UnboundedSender<Request>) -> Result<()> {
    gtk::init().context("initialising GTK (is WAYLAND_DISPLAY set?)")?;
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(true);
    }
    load_css()?;

    let store = Rc::new(Store::default());
    let tray = tray::Tray::new(reqs.clone());
    let picker = picker::Picker::new(cfg.clone());
    let notifier = notify::Notifier::new(cfg.clone(), reqs.clone());
    let display = gdk::Display::default().context("no display")?;
    let outputs: Rc<RefCell<Vec<bar::OutputUi>>> = Rc::default();
    let next_id = Rc::new(std::cell::Cell::new(1u32));

    let open = {
        let (cfg, store, tray, outputs, reqs, next_id) = (
            cfg.clone(),
            store.clone(),
            tray.clone(),
            outputs.clone(),
            reqs.clone(),
            next_id.clone(),
        );
        Rc::new(move |monitor: &gdk::Monitor| {
            let id = next_id.get();
            next_id.set(id + 1);
            eprintln!("bar on {} ({id})", monitor.connector().map(|c| c.to_string()).unwrap_or_default());
            outputs
                .borrow_mut()
                .push(bar::build(&cfg, &store, &tray, monitor, id, reqs.clone()));
        })
    };
    let monitors = display.monitors();
    for i in 0..monitors.n_items() {
        if let Some(m) = monitors.item(i).and_downcast::<gdk::Monitor>() {
            open(&m);
        }
    }
    {
        let (store, tray, outputs) = (store.clone(), tray.clone(), outputs.clone());
        monitors.connect_items_changed(move |model: &gio::ListModel, pos, removed, added| {
            // the model is the truth: a bar whose monitor object is no
            // longer listed goes (sway's reload announces the output anew,
            // as a new object, before the old one is invalid)
            let listed: Vec<gdk::Monitor> = (0..model.n_items())
                .filter_map(|i| model.item(i).and_downcast::<gdk::Monitor>())
                .collect();
            eprintln!(
                "monitors changed: pos {pos}, removed {removed}, added {added}; now {:?}",
                listed
                    .iter()
                    .map(|m| m.connector().map(|c| c.to_string()).unwrap_or_default())
                    .collect::<Vec<_>>()
            );
            outputs.borrow_mut().retain(|o| {
                if o.monitor.is_valid() && listed.contains(&o.monitor) {
                    return true;
                }
                eprintln!("bar off {}", o.id);
                tray.drop_owner(o.id);
                o.close(&store);
                false
            });
            let have: Vec<gdk::Monitor> = outputs.borrow().iter().map(|o| o.monitor.clone()).collect();
            for m in listed {
                if !have.contains(&m) {
                    open(&m);
                }
            }
        });
    }

    // the sources' values come in here; the channel closing is the quit
    let main_loop = glib::MainLoop::new(None, false);
    let quit = main_loop.clone();
    glib::MainContext::default().spawn_local(async move {
        while let Ok(msg) = updates.recv().await {
            match msg {
                Message::Var(u) => store.set(&u.name, u.value),
                Message::Tray(u) => tray.handle(*u),
                Message::Pick(r) => picker.show(*r),
                Message::CancelPick => picker.cancel(),
                Message::Notify(u) => notifier.handle(u),
            }
        }
        quit.quit();
    });
    main_loop.run();
    Ok(())
}

fn load_css() -> Result<()> {
    let provider = gtk::CssProvider::new();
    let path = config_dir().join("style.css");
    match std::fs::read_to_string(&path) {
        Ok(text) => provider.load_from_string(&text),
        Err(_) => provider.load_from_string(DEFAULT_CSS),
    }
    let display = gdk::Display::default().context("no display")?;
    gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    Ok(())
}

/// An icon at `size` logical pixels: a file, or a themed name the theme
/// has; otherwise an empty space of that size, so lines stay aligned.
/// GTK 4 renders it for the surface's own scale, fractional included.
pub fn icon_image(icon: &str, size: i32) -> gtk::Image {
    let image = if icon.starts_with('/') {
        match gdk::Texture::from_filename(icon) {
            Ok(t) => gtk::Image::from_paintable(Some(&t)),
            Err(_) => gtk::Image::new(),
        }
    } else if !icon.is_empty() && icon_exists(icon) {
        gtk::Image::from_icon_name(icon)
    } else {
        gtk::Image::new()
    };
    image.set_pixel_size(size);
    image
}

pub fn icon_exists(icon: &str) -> bool {
    if icon.starts_with('/') {
        return std::path::Path::new(icon).is_file();
    }
    gdk::Display::default()
        .map(|d| gtk::IconTheme::for_display(&d).has_icon(icon))
        .unwrap_or(false)
}
