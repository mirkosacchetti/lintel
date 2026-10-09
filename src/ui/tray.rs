//! The tray on screen: one icon per item in every bar's tray box, the
//! tooltip, the mouse (Activate, SecondaryActivate, Scroll over D-Bus
//! through the sources side) and the menu: the item's dbusmenu layout,
//! fetched on the sources side when asked, shown as a popover menu.

use crate::ipc::Request;
use crate::sources::tray::{Icon, MenuNode, TrayCommand, TrayItem, TrayUpdate};
use gtk::gdk;
use gtk::gdk_pixbuf::{Colorspace, Pixbuf};
use gtk::gio;
use gtk::glib;
use gtk::prelude::*;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::{Rc, Weak};
use tokio::sync::mpsc::UnboundedSender;

pub struct Tray {
    items: RefCell<BTreeMap<String, TrayItem>>,
    /// The tray boxes of the open bars: owner (the output's id), the box,
    /// its icon size.
    boxes: RefCell<Vec<(u32, gtk::Box, i32)>>,
    /// The icon the menu was asked from, to pop it up there.
    anchors: RefCell<HashMap<String, gtk::Widget>>,
    theme_paths: RefCell<HashSet<String>>,
    reqs: UnboundedSender<Request>,
}

impl Tray {
    pub fn new(reqs: UnboundedSender<Request>) -> Rc<Tray> {
        Rc::new(Tray {
            items: RefCell::default(),
            boxes: RefCell::default(),
            anchors: RefCell::default(),
            theme_paths: RefCell::default(),
            reqs,
        })
    }

    pub fn handle(self: &Rc<Self>, update: TrayUpdate) {
        match update {
            TrayUpdate::Item(item) => {
                let item = *item;
                if !item.icon_theme_path.is_empty() && self.theme_paths.borrow_mut().insert(item.icon_theme_path.clone()) {
                    if let Some(d) = gdk::Display::default() {
                        gtk::IconTheme::for_display(&d).add_search_path(&item.icon_theme_path);
                    }
                }
                self.items.borrow_mut().insert(item.key.clone(), item);
                self.rebuild_all();
            }
            TrayUpdate::Remove(key) => {
                self.items.borrow_mut().remove(&key);
                self.anchors.borrow_mut().remove(&key);
                self.rebuild_all();
            }
            TrayUpdate::Menu { key, root } => self.popup_menu(&key, &root),
        }
    }

    pub fn add_box(self: &Rc<Self>, owner: u32, b: gtk::Box, icon_size: i32) {
        self.rebuild(&b, icon_size);
        self.boxes.borrow_mut().push((owner, b, icon_size));
    }

    pub fn drop_owner(&self, owner: u32) {
        self.boxes.borrow_mut().retain(|(o, _, _)| *o != owner);
    }

    fn rebuild_all(self: &Rc<Self>) {
        let boxes: Vec<(gtk::Box, i32)> = self.boxes.borrow().iter().map(|(_, b, s)| (b.clone(), *s)).collect();
        for (b, size) in boxes {
            self.rebuild(&b, size);
        }
    }

    fn rebuild(self: &Rc<Self>, b: &gtk::Box, size: i32) {
        while let Some(c) = b.first_child() {
            b.remove(&c);
        }
        let items = self.items.borrow();
        for item in items.values() {
            if item.status == "Passive" {
                continue;
            }
            b.append(&self.icon_widget(item, size));
        }
    }

    fn icon_widget(self: &Rc<Self>, item: &TrayItem, size: i32) -> gtk::Box {
        let icon = if item.status == "NeedsAttention" && !item.attention_icon.is_none() {
            &item.attention_icon
        } else {
            &item.icon
        };
        let image = image_for(icon, size);
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("tray-item");
        root.append(&image);
        let tip = if item.tooltip.is_empty() { &item.title } else { &item.tooltip };
        if !tip.is_empty() {
            root.set_tooltip_text(Some(tip));
        }

        let key = item.key.clone();
        let item_is_menu = item.item_is_menu;
        let has_menu = item.menu.is_some();
        let tray: Weak<Tray> = Rc::downgrade(self);
        let gesture = gtk::GestureClick::new();
        gesture.set_button(0);
        gesture.connect_pressed(move |g, _, _, _| {
            let Some(tray) = tray.upgrade() else { return };
            let Some(w) = g.widget() else { return };
            let open_menu = || {
                tray.anchors.borrow_mut().insert(key.clone(), w.clone());
                let _ = tray.reqs.send(Request::Tray(TrayCommand::MenuOpen { key: key.clone() }));
            };
            let cmd = match g.current_button() {
                1 if item_is_menu && has_menu => {
                    open_menu();
                    None
                }
                1 => Some(TrayCommand::Activate {
                    key: key.clone(),
                    x: 0,
                    y: 0,
                }),
                2 => Some(TrayCommand::SecondaryActivate {
                    key: key.clone(),
                    x: 0,
                    y: 0,
                }),
                3 if has_menu => {
                    open_menu();
                    None
                }
                3 => Some(TrayCommand::ContextMenu {
                    key: key.clone(),
                    x: 0,
                    y: 0,
                }),
                _ => None,
            };
            if let Some(cmd) = cmd {
                let _ = tray.reqs.send(Request::Tray(cmd));
            }
        });
        root.add_controller(gesture);
        let key = item.key.clone();
        let reqs = self.reqs.clone();
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(move |_, _, dy| {
            let delta = if dy < 0.0 {
                -1
            } else if dy > 0.0 {
                1
            } else {
                0
            };
            if delta != 0 {
                let _ = reqs.send(Request::Tray(TrayCommand::Scroll {
                    key: key.clone(),
                    delta,
                    orientation: "vertical".into(),
                }));
            }
            glib::Propagation::Stop
        });
        root.add_controller(scroll);
        root
    }

    /// The item's menu, as the dbusmenu layout came: a popover on the icon
    /// it was asked from. Each entry is an action that sends the item its
    /// "clicked" event.
    fn popup_menu(self: &Rc<Self>, key: &str, root: &MenuNode) {
        let Some(anchor) = self.anchors.borrow().get(key).cloned() else {
            return;
        };
        let actions = gio::SimpleActionGroup::new();
        let model = gio::Menu::new();
        self.fill_menu(&model, &actions, key, &root.children);
        anchor.insert_action_group("tray", Some(&actions));
        let popover = gtk::PopoverMenu::from_model(Some(&model));
        popover.add_css_class("tray-menu");
        popover.set_has_arrow(false);
        popover.set_parent(&anchor);
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn fill_menu(self: &Rc<Self>, menu: &gio::Menu, actions: &gio::SimpleActionGroup, key: &str, nodes: &[MenuNode]) {
        let mut section = gio::Menu::new();
        for n in nodes {
            if !n.visible {
                continue;
            }
            if n.separator {
                if section.n_items() > 0 {
                    menu.append_section(None, &section);
                    section = gio::Menu::new();
                }
                continue;
            }
            let label = if n.toggle_type.is_empty() {
                n.label.clone()
            } else if n.toggle_state == 1 {
                format!("✓ {}", n.label)
            } else {
                format!("   {}", n.label)
            };
            if !n.children.is_empty() {
                let sub = gio::Menu::new();
                self.fill_menu(&sub, actions, key, &n.children);
                section.append_submenu(Some(&label), &sub);
                continue;
            }
            let name = format!("item{}", n.id);
            let action = gio::SimpleAction::new(&name, None);
            action.set_enabled(n.enabled);
            let (reqs, key, id) = (self.reqs.clone(), key.to_string(), n.id);
            action.connect_activate(move |_, _| {
                let _ = reqs.send(Request::Tray(TrayCommand::MenuEvent { key: key.clone(), id }));
            });
            actions.add_action(&action);
            section.append(Some(&label), Some(&format!("tray.{name}")));
        }
        if section.n_items() > 0 {
            menu.append_section(None, &section);
        }
    }
}

/// The icon as a GTK image at `size` logical pixels: a themed name, a
/// file, or the item's own pixmap as a texture.
fn image_for(icon: &Icon, size: i32) -> gtk::Image {
    let image = match icon {
        Icon::Pixmap { width, height, rgba } => {
            let pb = Pixbuf::from_mut_slice(rgba.clone(), Colorspace::Rgb, true, 8, *width, *height, width * 4);
            gtk::Image::from_paintable(Some(&gdk::Texture::for_pixbuf(&pb)))
        }
        Icon::Name(name) => return super::icon_image(name, size),
        Icon::None => gtk::Image::new(),
    };
    image.set_pixel_size(size);
    image
}
