//! The notifications on screen: a layer-shell window at the top (left,
//! centre or right, under the bar) holding one card per notification:
//! the icon or image, the summary, the body, a progress bar when the app
//! sends one, the actions as buttons. Left click activates (the app's
//! default action, then its window), right click closes, middle click
//! closes them all. The window is only mapped while something shows.

use crate::config::Config;
use crate::ipc::Request;
use crate::sources::notifyd::{Image, Note, NotifyEvent, NotifyUpdate};
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

pub struct Notifier {
    cfg: Arc<Config>,
    window: gtk::Window,
    list: gtk::Box,
    cards: RefCell<HashMap<u32, gtk::Box>>,
    reqs: UnboundedSender<Request>,
}

impl Notifier {
    pub fn new(cfg: Arc<Config>, reqs: UnboundedSender<Request>) -> Rc<Notifier> {
        let window = gtk::Window::new();
        window.set_title(Some("lintel notifications"));
        window.add_css_class("lintel-notifications");
        window.set_decorated(false);
        window.init_layer_shell();
        window.set_namespace(Some("lintel-notifications"));
        window.set_layer(Layer::Overlay);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_exclusive_zone(-1);
        window.set_anchor(Edge::Top, true);
        match cfg.notifications.position.as_str() {
            "top-left" => window.set_anchor(Edge::Left, true),
            "top-right" => window.set_anchor(Edge::Right, true),
            _ => {}
        }
        window.set_margin(Edge::Top, cfg.bar.height + cfg.notifications.offset);
        window.set_margin(Edge::Left, cfg.notifications.offset);
        window.set_margin(Edge::Right, cfg.notifications.offset);
        let list = gtk::Box::new(gtk::Orientation::Vertical, cfg.notifications.gap);
        list.add_css_class("notifications");
        list.set_size_request(cfg.notifications.width, -1);
        window.set_child(Some(&list));
        Rc::new(Notifier {
            cfg,
            window,
            list,
            cards: RefCell::default(),
            reqs,
        })
    }

    pub fn handle(self: &Rc<Self>, update: NotifyUpdate) {
        match update {
            NotifyUpdate::Show(n) => {
                let card = self.card(&n);
                if let Some(old) = self.cards.borrow_mut().remove(&n.id) {
                    // replaced in place: same spot in the stack
                    self.list.insert_child_after(&card, Some(&old));
                    self.list.remove(&old);
                } else {
                    self.list.append(&card);
                }
                self.cards.borrow_mut().insert(n.id, card);
            }
            NotifyUpdate::Close(id) => {
                if let Some(card) = self.cards.borrow_mut().remove(&id) {
                    self.list.remove(&card);
                }
            }
        }
        if self.cards.borrow().is_empty() {
            self.window.set_visible(false);
        } else if !self.window.is_visible() {
            self.window.present();
        }
    }

    fn send(&self, ev: NotifyEvent) {
        let _ = self.reqs.send(Request::Notify(ev));
    }

    fn card(self: &Rc<Self>, n: &Note) -> gtk::Box {
        let size = self.cfg.notifications.icon_size;
        let card = gtk::Box::new(gtk::Orientation::Vertical, 0);
        card.add_css_class("notification");
        card.add_css_class(match n.urgency {
            0 => "low",
            2 => "critical",
            _ => "normal",
        });
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        row.add_css_class("inner");
        if let Some(image) = n.image.as_ref().map(|i| image_widget(i, size)) {
            image.set_valign(gtk::Align::Start);
            row.append(&image);
        } else if !n.icon.is_empty() && super::icon_exists(&n.icon) {
            // a name the theme has not got means no icon, not a placeholder
            let image = super::icon_image(&n.icon, size);
            image.set_valign(gtk::Align::Start);
            row.append(&image);
        }
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_hexpand(true);
        let summary = gtk::Label::new(None);
        summary.add_css_class("summary");
        summary.set_xalign(0.0);
        summary.set_wrap(true);
        summary.set_text(&n.summary);
        text.append(&summary);
        if !n.body.trim().is_empty() {
            let body = gtk::Label::new(None);
            body.add_css_class("body");
            body.set_xalign(0.0);
            body.set_wrap(true);
            body.set_max_width_chars(44);
            body.set_lines(8);
            body.set_ellipsize(gtk::pango::EllipsizeMode::End);
            // markup when it parses, text otherwise
            match gtk::pango::parse_markup(&n.body, '\0') {
                Ok(_) => body.set_markup(&n.body),
                Err(_) => body.set_text(&n.body),
            }
            text.append(&body);
        }
        if let Some(v) = n.value {
            let bar = gtk::ProgressBar::new();
            bar.set_fraction((v.clamp(0, 100) as f64) / 100.0);
            text.append(&bar);
        }
        let buttons: Vec<&(String, String)> = n.actions.iter().filter(|(k, _)| k != "default").collect();
        if !buttons.is_empty() {
            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.add_css_class("actions");
            for (key, label) in buttons {
                let b = gtk::Button::with_label(label);
                // on the press, claimed: the card's own click (which closes
                // the notification) must not see it first
                let (me, id, key) = (self.clone(), n.id, key.clone());
                let g = gtk::GestureClick::new();
                g.connect_pressed(move |g, _, _, _| {
                    g.set_state(gtk::EventSequenceState::Claimed);
                    me.send(NotifyEvent::Action(id, key.clone()));
                });
                b.add_controller(g);
                actions.append(&b);
            }
            text.append(&actions);
        }
        row.append(&text);
        card.append(&row);
        let (me, id) = (self.clone(), n.id);
        let gesture = gtk::GestureClick::new();
        gesture.set_button(0);
        gesture.connect_pressed(move |g, _, _, _| match g.current_button() {
            1 => me.send(NotifyEvent::Clicked(id)),
            2 => me.send(NotifyEvent::DismissAll),
            3 => me.send(NotifyEvent::Dismiss(id)),
            _ => {}
        });
        card.add_controller(gesture);
        card
    }
}

/// The image-data hint as a texture, `size` pixels on its longer side.
fn image_widget(i: &Image, size: i32) -> gtk::Image {
    let pb = Pixbuf::from_mut_slice(i.data.clone(), Colorspace::Rgb, i.has_alpha, 8, i.width, i.height, i.rowstride);
    let image = gtk::Image::from_paintable(Some(&gdk::Texture::for_pixbuf(&pb)));
    image.set_pixel_size(size);
    image
}
