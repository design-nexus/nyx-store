#[path = "blackarch_dialog.rs"]
mod blackarch_dialog;
#[path = "home.rs"]
mod home;
use crate::{prefs, theme, widgets};
use gtk::{gdk, gio, glib, prelude::*};
use nyx_store::{
    aur::{self, Recipe},
    catalog::{self, Catalog},
    model::{App, Event, Plan, Request},
    repositories, transaction,
};
use nyx_ui::chrome;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, BTreeSet, VecDeque},
    rc::Rc,
    sync::mpsc,
};

const PAGES: &[(&str, &str, &str)] = &[
    ("home", "Home", "go-home-symbolic"),
    // NyxOS only; its icon is the system logo.
    ("nyx", "Nyx Apps", ""),
    ("categories", "Categories", "view-grid-symbolic"),
    ("installed", "Installed", "emblem-default-symbolic"),
    ("updates", "Updates", "software-update-available-symbolic"),
    ("repositories", "Repositories", "network-server-symbolic"),
];
pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["Ctrl", "F"], "Search apps"),
    (&["Ctrl", "B"], "Collapse sidebar"),
    (&["Alt", "Left"], "Back to Home"),
    (&["Ctrl", "R"], "Refresh catalog"),
    (&["Ctrl", ","], "Settings"),
    (&["Ctrl", "Shift", "P"], "Command palette"),
    (&["Esc"], "Close panel or clear search"),
    (&["F1"], "Keyboard shortcuts"),
    (&["Ctrl", "Q"], "Close Store"),
];

struct Ui {
    window: gtk::ApplicationWindow,
    overlay: gtk::Overlay,
    nav: gtk::Box,
    header: chrome::AppHeader,
    nav_buttons: Vec<(String, gtk::Button)>,
    /// A count beside a sidebar item (the updates available).
    nav_badges: Vec<(String, gtk::Label)>,
    body: gtk::Box,
    scroll: gtk::ScrolledWindow,
    search: gtk::SearchEntry,
    /// The search's source button: `None` for all, `nyx` for Nyx apps, else a source.
    /// It only filters search results and resets when the search is cleared.
    search_source: RefCell<Option<String>>,
    /// Include packages: libraries and command-line packages in search results.
    include_packages: Cell<bool>,
    /// The page the search was started from, to go back to when it's cleared.
    return_page: RefCell<String>,
    /// The nixpkgs or AUR search is running, or what went wrong with it.
    searching: Cell<bool>,
    search_error: RefCell<String>,
    /// NyxOS: nyx update's last check (when, and what it found).
    system_updates: RefCell<Option<(i64, Vec<nyx_store::model::Change>)>>,
    status: gtk::Label,
    notice: gtk::Label,
    spinner: gtk::Spinner,
    progress: gtk::ProgressBar,
    toast: gtk::Label,
    catalog: RefCell<Option<Catalog>>,
    page: RefCell<String>,
    category: RefCell<String>,
    aur: RefCell<Vec<App>>,
    aur_updates: RefCell<Vec<App>>,
    search_generation: Cell<u64>,
    busy: Cell<bool>,
    loading: Cell<bool>,
    committed: Cell<bool>,
    cancel_button: gtk::Button,
    cancelled: RefCell<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    confirmation: RefCell<Option<mpsc::Sender<Option<Plan>>>>,
    pending_recipe: RefCell<Option<Recipe>>,
    queue: RefCell<VecDeque<Request>>,
    /// A launcher's desktop id from `--remove`, until the catalog has loaded.
    pending_remove: RefCell<Option<String>>,
}
thread_local! { static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) }; }
fn ui() -> Rc<Ui> {
    UI.with(|u| u.borrow().as_ref().unwrap().clone())
}

pub fn present(app: &gtk::Application, section: Option<&str>, query: Option<&str>, remove: Option<&str>) {
    if UI.with(|u| u.borrow().is_none()) {
        build(app);
        let cache = nyx_core::paths::cache_dir().join("catalog-refreshed");
        let p = prefs::get();
        let due = p.catalog_refresh == "startup" || (p.catalog_refresh == "daily" &&
            cache.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).is_none_or(|age| age.as_secs() >= 86400));
        refresh(due);
        glib::timeout_add_seconds_local(60, || {
            let marker = nyx_core::paths::cache_dir().join("catalog-refreshed");
            if prefs::get().catalog_refresh == "daily" && marker.metadata().and_then(|m| m.modified()).ok()
                .and_then(|t| t.elapsed().ok()).is_none_or(|age| age.as_secs() >= 86400) {
                refresh(true);
            }
            glib::ControlFlow::Continue
        });
    }
    let u = ui();
    if let Some(section) = section {
        navigate(section);
    }
    if let Some(q) = query {
        u.search.set_text(q);
        navigate("search");
    }
    u.window.present();
    if let Some(id) = remove {
        *u.pending_remove.borrow_mut() = Some(id.to_string());
        navigate("installed");
        if u.catalog.borrow().is_some() && !u.loading.get() {
            offer_removal();
        }
    }
}

/// `--remove`: the details of the installed app the launcher belongs to, whose Remove
/// goes through the usual review.
fn offer_removal() {
    let u = ui();
    let Some(id) = u.pending_remove.borrow_mut().take() else { return };
    let owner = launcher_owner(&id);
    let found = u.catalog.borrow().as_ref().and_then(|c| nyx_store::model::installed_app(&c.apps, &id, owner.as_deref()).cloned());
    match found {
        Some(app) => details(app),
        None => notice("This app wasn't installed through a source Store manages."),
    }
}

/// The package that owns a launcher's file: pacman's owner, or the Nix store path's name.
/// A copy in the user's own folder (an edited launcher) belongs to no package, so each
/// copy is tried.
fn launcher_owner(id: &str) -> Option<String> {
    let file = format!("applications/{}.desktop", id.trim_end_matches(".desktop"));
    let nix = nyx_store::nix::active();
    std::iter::once(glib::user_data_dir())
        .chain(glib::system_data_dirs())
        .map(|d| d.join(&file))
        .filter(|p| p.exists())
        .find_map(|path| {
            if nix {
                let real = std::fs::canonicalize(&path).ok()?;
                return nyx_store::model::nix_store_name(&real.to_string_lossy());
            }
            let out = std::process::Command::new("/usr/bin/pacman").arg("-Qqo").arg(&path).output().ok()?;
            let name = String::from_utf8_lossy(&out.stdout).lines().next()?.trim().to_string();
            (out.status.success() && !name.is_empty()).then_some(name)
        })
}

/// The search's width in characters, in a full window and a narrow one.
const SEARCH_WIDE: i32 = 30;
const SEARCH_NARROW: i32 = 14;

fn build(app: &gtk::Application) {
    theme::install();
    let window =
        gtk::ApplicationWindow::builder().application(app).default_width(1280).default_height(800).title("Store").build();
    window.set_titlebar(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    window.add_css_class("store-window");
    let overlay = gtk::Overlay::new();
    let frame = widgets::vbox(0);
    frame.add_css_class("window-frame");
    frame.add_css_class("store-frame");
    let body_row = widgets::hbox(0);
    body_row.set_vexpand(true);
    let nav = widgets::vbox(0);
    nav.add_css_class("settings-navigation");
    nav.set_hexpand(false);
    let list = widgets::vbox(0);
    list.set_hexpand(false);
    let mut nav_buttons = Vec::new();
    let mut nav_badges = Vec::new();
    for &(id, title, icon) in PAGES.iter().filter(|p| page_available(p.0)) {
        let button = gtk::Button::new();
        button.add_css_class("nav-item");
        button.set_tooltip_text(Some(title));
        let row = widgets::hbox(12);
        if icon.is_empty() {
            row.append(&widgets::logo(16));
        } else {
            row.append(&gtk::Image::from_icon_name(icon));
        }
        let label = widgets::label(title, "nav-label");
        label.add_css_class("compact-hide");
        label.set_hexpand(true);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_xalign(0.0);
        row.append(&label);
        let badge = widgets::label("", "nav-badge");
        badge.add_css_class("compact-hide");
        badge.set_visible(false);
        badge.set_valign(gtk::Align::Center);
        row.append(&badge);
        button.set_child(Some(&row));
        button.connect_clicked(move |_| navigate(id));
        list.append(&button);
        nav_buttons.push((id.into(), button));
        nav_badges.push((id.into(), badge));
    }
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hexpand(false)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&list)
        .build();
    nav.append(&nav_scroll);
    body_row.append(&nav);
    let column = widgets::vbox(0);
    column.add_css_class("settings-content");
    column.set_hexpand(true);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some(&format!("{}  (Ctrl+F)", search_label())));
    search.add_css_class("bar-search");
    search.set_width_chars(SEARCH_WIDE);
    search.update_property(&[gtk::accessible::Property::Label(search_label())]);
    search.connect_stop_search(|e| e.set_text(""));
    let notice_label = widgets::label("", "store-notice");
    notice_label.set_wrap(true);
    notice_label.set_xalign(0.0);
    notice_label.set_visible(false);
    column.append(&notice_label);
    let progress = gtk::ProgressBar::new();
    progress.add_css_class("store-progress");
    progress.set_visible(false);
    column.append(&progress);
    let body = widgets::vbox(0);
    body.add_css_class("settings-page");
    body.set_hexpand(true);
    let scroll = gtk::ScrolledWindow::builder()
        .child(&body)
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .build();
    column.append(&scroll);
    body_row.append(&column);

    // ----- Header -----
    // The app and the page, the sidebar toggle; the search in the middle; what the
    // catalog holds or the operation under way, Refresh and Settings at the end.
    let header = chrome::app_header(&window, "system-software-install-symbolic", "Store");
    let collapse = widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.connect_clicked(|_| toggle_sidebar());
    header.start.append(&collapse);
    header.center.append(&search);
    header.end.set_spacing(4);
    let status = widgets::label("Loading apps…", "header-status");
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);
    status.set_max_width_chars(48);
    status.set_margin_end(6);
    header.end.append(&status);
    let spinner = gtk::Spinner::new();
    header.end.append(&spinner);
    let cancel_button = widgets::bar_button("process-stop-symbolic", "Cancel before installation");
    cancel_button.connect_clicked(|_| cancel_operation());
    cancel_button.set_visible(false);
    header.end.append(&cancel_button);
    let refresh_button = widgets::bar_button("view-refresh-symbolic", "Refresh catalog (Ctrl+R)");
    refresh_button.connect_clicked(|_| refresh(true));
    header.end.append(&refresh_button);
    header.end.append(&chrome::settings_button(settings));
    frame.append(&header.root);
    frame.append(&body_row);
    overlay.set_child(Some(&frame));
    let toast_label = widgets::label("", "store-toast");
    toast_label.add_css_class("toast");
    toast_label.set_wrap(true);
    toast_label.set_halign(gtk::Align::Center);
    toast_label.set_valign(gtk::Align::End);
    toast_label.set_margin_bottom(48);
    toast_label.set_visible(false);
    overlay.add_overlay(&toast_label);
    window.set_child(Some(&overlay));
    let p = prefs::get();
    let first = nyx_theme::start_page("home", &p.last_section);
    let u = Rc::new(Ui {
        window: window.clone(),
        overlay: overlay.clone(),
        nav: nav.clone(),
        header,
        nav_buttons,
        nav_badges,
        body,
        scroll,
        search: search.clone(),
        search_source: RefCell::new(None),
        include_packages: Cell::new(false),
        return_page: RefCell::new("home".into()),
        searching: Cell::new(false),
        search_error: RefCell::new(String::new()),
        system_updates: RefCell::new(None),
        status,
        notice: notice_label,
        spinner,
        progress,
        toast: toast_label,
        catalog: RefCell::new(None),
        page: RefCell::new(first),
        category: RefCell::new(String::new()),
        aur: RefCell::new(Vec::new()),
        aur_updates: RefCell::new(Vec::new()),
        search_generation: Cell::new(0),
        busy: Cell::new(false),
        loading: Cell::new(false),
        committed: Cell::new(false),
        cancel_button,
        cancelled: RefCell::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))),
        confirmation: RefCell::new(None),
        pending_recipe: RefCell::new(None),
        queue: RefCell::new(VecDeque::new()),
        pending_remove: RefCell::new(None),
    });
    UI.with(|v| *v.borrow_mut() = Some(u.clone()));
    search.connect_search_changed(|entry| {
        let u = ui();
        let page = u.page.borrow().clone();
        if !entry.text().is_empty() {
            if page != "search" {
                *u.return_page.borrow_mut() = page;
                *u.page.borrow_mut() = "search".into();
            }
        } else if page == "search" {
            let back = u.return_page.borrow().clone();
            *u.page.borrow_mut() = if back.is_empty() || back == "search" { "home".into() } else { back };
            u.search_source.borrow_mut().take();
        }
        u.aur.borrow_mut().clear();
        u.search_error.borrow_mut().clear();
        let generation = u.search_generation.get() + 1;
        u.search_generation.set(generation);
        let query = entry.text().to_string();
        let remote = prefs::get().include_aur && query.chars().count() >= 2;
        u.searching.set(remote);
        render();
        if remote {
            glib::timeout_add_local_once(std::time::Duration::from_millis(350), move || {
                if !prefs::get().include_aur || ui().search_generation.get() != generation {
                    return;
                }
                let (tx, rx) = async_channel::bounded(1);
                std::thread::spawn(move || {
                    let search = if nyx_store::nix::active() { nyx_store::nix::search } else { aur::search };
                    let _ = tx.send_blocking(search(&query).map_err(|e| e.to_string()));
                });
                glib::spawn_future_local(async move {
                    if let Ok(result) = rx.recv().await
                        && prefs::get().include_aur && ui().search_generation.get() == generation {
                            ui().searching.set(false);
                            match result {
                                Ok(mut apps) => {
                                    if let Some(c) = ui().catalog.borrow().as_ref() {
                                        for a in &mut apps {
                                            a.installed = c.installed.get(&a.package).map(|p| p.version.clone());
                                            let base = a.package.trim_end_matches("-bin").trim_end_matches("-git");
                                            if let Some(meta) = c.apps.iter().find(|p| p.desktop && p.package == base) {
                                                a.desktop = true;
                                                a.icon = meta.icon.clone();
                                                a.category = meta.category.clone();
                                            }
                                        }
                                    }
                                    *ui().aur.borrow_mut() = apps;
                                }
                                Err(e) => *ui().search_error.borrow_mut() = e,
                            }
                            render();
                        }
                });
            });
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed(|_, key, _, mods| {
        let ctrl = mods.contains(gdk::ModifierType::CONTROL_MASK);
        match key {
            gdk::Key::f if ctrl => find(),
            gdk::Key::b if ctrl => toggle_sidebar(),
            gdk::Key::r if ctrl => refresh(true),
            gdk::Key::F1 => shortcuts(),
            gdk::Key::Left if mods.contains(gdk::ModifierType::ALT_MASK) => navigate("home"),
            gdk::Key::Escape => {
                if nyx_ui::settings_card::is_open() {
                    nyx_ui::settings_card::escape();
                } else {
                    let u = ui();
                    gtk::prelude::GtkWindowExt::set_focus(&u.window, None::<&gtk::Widget>);
                    if u.search.text().is_empty() {
                        navigate("home");
                    } else {
                        u.search.set_text("");
                    }
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
    nyx_ui::keys::install(
        &window,
        nyx_ui::keys::Common { settings: Some(settings), shortcuts: Some(shortcuts), close: true, ..Default::default() },
    );
    window.connect_close_request(|_| {
        if ui().busy.get() {
            toast("Wait for the current operation to finish, or cancel its review.");
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    let probe = gtk::DrawingArea::new();
    probe.set_can_target(false);
    probe.set_can_focus(false);
    overlay.add_overlay(&probe);
    overlay.set_measure_overlay(&probe, false);
    probe.connect_resize(|_, _, _| {
        glib::idle_add_local_once(responsive);
    });
    window.connect_default_width_notify(|_| responsive());
    window.connect_realize(|_| responsive());
    let weak = window.downgrade();
    glib::timeout_add_local(std::time::Duration::from_millis(1500), move || {
        if weak.upgrade().is_none() {
            return glib::ControlFlow::Break;
        }
        responsive();
        glib::ControlFlow::Continue
    });
    responsive();
    render();
}

fn find() {
    ui().search.grab_focus();
}
fn toggle_sidebar() {
    prefs::update(|p| p.sidebar_collapsed = !p.sidebar_collapsed);
    responsive();
}
fn responsive() {
    let u = ui();
    let width = if u.window.width() > 0 { u.window.width() } else { u.window.default_width() };
    let compact = (width > 0 && width < 980) || prefs::get().sidebar_collapsed;
    nyx_ui::settings_card::fit(&u.window);
    if compact {
        u.nav.add_css_class("compact");
    } else {
        u.nav.remove_css_class("compact");
    }
    fn hide(widget: &gtk::Widget, compact: bool) {
        if widget.has_css_class("nav-item")
            && let Some(content) = widget.downcast_ref::<gtk::Button>().and_then(|b| b.child()) {
                content.set_halign(if compact { gtk::Align::Center } else { gtk::Align::Fill });
            }
        let mut c = widget.first_child();
        while let Some(child) = c {
            if child.has_css_class("compact-hide") {
                child.set_visible(!compact);
            }
            hide(&child, compact);
            c = child.next_sibling();
        }
    }
    hide(u.nav.upcast_ref(), compact);
    show_badges();
    // Narrow windows (a tiled half-screen) get a shorter search and no catalog counts.
    let narrow = width > 0 && width < 980;
    if narrow {
        u.body.add_css_class("narrow");
    } else {
        u.body.remove_css_class("narrow");
    }
    let columns = grid_columns();
    let mut child = u.body.first_child();
    while let Some(c) = child {
        if let Some(grid) = c.downcast_ref::<gtk::FlowBox>().filter(|g| g.has_css_class("store-grid")) {
            grid.set_min_children_per_line(columns);
        }
        child = c.next_sibling();
    }
    u.search.set_width_chars(if narrow { SEARCH_NARROW } else { SEARCH_WIDE });
    u.search.set_placeholder_text(Some(&if narrow { "Search apps".to_string() } else { format!("{}  (Ctrl+F)", search_label()) }));
    u.status.set_visible(!narrow || u.busy.get() || u.loading.get());
}
/// Nyx Apps is NyxOS's; Repositories is Arch's (NyxOS's sources need no managing).
fn page_available(page: &str) -> bool {
    match page {
        "nyx" => nyx_store::nix::active(),
        "repositories" => !nyx_store::nix::active(),
        _ => true,
    }
}
fn navigate(page: &str) {
    if page == "settings" {
        settings();
        return;
    }
    let u = ui();
    *u.page.borrow_mut() = page.into();
    u.category.borrow_mut().clear();
    if page != "search" {
        u.search.set_text("");
        u.search_source.borrow_mut().take();
        *u.page.borrow_mut() = page.into();
    }
    prefs::update(|p| p.last_section = page.into());
    u.scroll.vadjustment().set_value(0.0);
    render();
}
fn notice(message: &str) {
    let u = ui();
    u.notice.set_text(message);
    u.notice.set_visible(!message.is_empty());
}
fn toast(message: &str) {
    let u = ui();
    u.toast.set_text(message);
    u.toast.set_visible(true);
    glib::timeout_add_local_once(std::time::Duration::from_millis(6000), move || u.toast.set_visible(false));
}
fn busy(on: bool) {
    let u = ui();
    u.busy.set(on);
    u.cancel_button.set_visible(on && !u.committed.get());
    u.spinner.set_spinning(on || u.loading.get());
    u.progress.set_visible(on);
    u.status.set_visible(on || !u.body.has_css_class("narrow"));
    if on {
        u.progress.pulse();
    }
}

fn refresh(network: bool) {
    let u = ui();
    if u.busy.get() || u.loading.replace(true) {
        return;
    }
    u.spinner.start();
    u.status.set_text(if network { "Refreshing apps and repository metadata…" } else { "Loading apps…" });
    notice("");
    let cache = nyx_core::paths::cache_dir();
    let bootstrap = !network && !cache.join("appstream.pkg.tar.zst").exists();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(catalog::load(&cache, network).map_err(|e| e.to_string()));
    });
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            let u = ui();
            u.loading.set(false);
            u.spinner.set_spinning(u.busy.get());
            match result {
                Ok(catalog) => {
                    let warning = catalog.warning.clone();
                    if network && warning.is_empty() {
                        let _ = std::fs::write(nyx_core::paths::cache_dir().join("catalog-refreshed"), b"");
                    }
                    *u.catalog.borrow_mut() = Some(catalog);
                    *u.system_updates.borrow_mut() =
                        if nyx_store::nix::active() { nyx_store::nix::checked_updates() } else { None };
                    notice(&warning);
                    render();
                    if bootstrap {
                        refresh(true);
                    } else {
                        offer_removal();
                    }
                }
                Err(e) => {
                    u.status.set_text("Catalog unavailable");
                    notice(&e);
                }
            }
        }
    });
}

fn heading(title: &str, note: &str) {
    let u = ui();
    let g = widgets::vbox(0);
    g.add_css_class("settings-group");
    let l = widgets::label(&title.to_uppercase(), "group-title");
    l.set_xalign(0.0);
    g.append(&l);
    if !note.is_empty() {
        let n = widgets::label(note, "collection-note");
        n.set_wrap(true);
        n.set_xalign(0.0);
        g.append(&n);
    }
    u.body.append(&g);
}
fn text(message: &str) {
    let l = widgets::label(message, "empty-state");
    l.set_wrap(true);
    ui().body.append(&l);
}
fn button(label: &str, action: impl Fn() + 'static) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.connect_clicked(move |_| action());
    b
}
/// The apps another app already offers (in its `also`), shown only under that app.
fn offered(apps: &[App]) -> BTreeSet<(String, String)> {
    apps.iter().flat_map(|a| a.also.iter().cloned()).collect()
}

/// The catalog's apps to browse: one entry per package, without the ones another
/// app already offers.
fn browsable(c: &Catalog) -> Vec<&App> {
    let offered = offered(&c.apps);
    let mut seen = BTreeSet::new();
    c.apps
        .iter()
        .filter(|a| !offered.contains(&(a.source.clone(), a.package.clone())))
        .filter(|a| seen.insert(a.package.as_str()))
        .collect()
}

/// A source as a short tag on a row.
fn source_tag(app: &App) -> Option<&'static str> {
    if app.nyx {
        return Some("Nyx");
    }
    match app.source.as_str() {
        "aur" => Some("AUR"),
        s if s == nyx_store::nix::NYXOS => Some("NyxOS"),
        s if s == nyx_store::nix::FLATHUB => Some("Flathub"),
        s if s == nyx_store::nix::NIXPKGS => Some("nixpkgs"),
        _ => None,
    }
}

fn render() {
    let u = ui();
    while let Some(c) = u.body.first_child() {
        u.body.remove(&c);
    }
    if !page_available(&u.page.borrow()) {
        *u.page.borrow_mut() = "home".into();
    }
    let page = u.page.borrow().clone();
    for (id, b) in &u.nav_buttons {
        if id == &page {
            b.add_css_class("active");
        } else {
            b.remove_css_class("active");
        }
    }
    let title = PAGES.iter().find(|p| p.0 == page).map(|p| p.1).unwrap_or("Search");
    chrome::set_title(&u.window, "Store", title);
    u.header.set_subtitle(title);
    let c = u.catalog.borrow();
    let Some(c) = c.as_ref() else {
        text("Loading your app catalog…");
        return;
    };
    update_badges(c);
    let category = u.category.borrow().clone();
    match page.as_str() {
        "repositories" => render_repositories(c),
        "updates" => render_updates(c),
        "nyx" => render_nyx(c),
        "categories" if category.is_empty() => render_categories(c),
        "categories" => render_category(c, &category),
        "installed" => render_installed(c),
        "search" => render_search(c),
        _ => home::render(c),
    }
    if !u.busy.get() {
        u.status.set_text(&format!(
            "{} apps · {} installed · {}",
            c.apps.iter().filter(|a| a.desktop).map(|a| &a.package).collect::<BTreeSet<_>>().len(),
            c.apps.iter().filter(|a| a.desktop && a.installed.is_some()).map(|a| &a.package).collect::<BTreeSet<_>>().len(),
            if nyx_store::nix::active() { "NyxOS catalog, nixpkgs and Flathub" } else { "Omarchy first" }
        ));
    }
}

/// The count of updates beside Updates in the sidebar: NyxOS's from `nyx update`'s last
/// check (the Flathub ones in the catalog when there's none), Arch's from the catalog
/// and the AUR check.
fn update_badges(c: &Catalog) {
    let u = ui();
    let count = if nyx_store::nix::active() {
        u.system_updates.borrow().as_ref().map(|(_, items)| items.len()).unwrap_or(c.updates.len())
    } else {
        c.updates.len() + u.aur_updates.borrow().len()
    };
    for (id, badge) in &u.nav_badges {
        let n = if id == "updates" { count } else { 0 };
        badge.set_text(&if n > 0 { n.to_string() } else { String::new() });
    }
    show_badges();
}

/// Badges show when they have a count and the sidebar has its labels.
fn show_badges() {
    let u = ui();
    let compact = u.nav.has_css_class("compact");
    for (_, badge) in &u.nav_badges {
        badge.set_visible(!compact && !badge.text().is_empty());
    }
}

/// A grid of tiles, for categories and bundles.
fn tile_grid() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(2)
        .max_children_per_line(4)
        .column_spacing(12)
        .row_spacing(12)
        .homogeneous(true)
        .build()
}

/// A tile with its chip, name and a line under it (a count), in `tone`.
fn tile(name: &str, line: &str, icon: &str, tone: &str, action: impl Fn() + 'static) -> gtk::Button {
    let tile = gtk::Button::new();
    tile.add_css_class("collection-tile");
    tile.add_css_class(&format!("tone-{tone}"));
    let row = widgets::hbox(12);
    row.append(&tone_chip(icon, tone, 36));
    let words = widgets::vbox(1);
    words.set_valign(gtk::Align::Center);
    let title = widgets::label(name, "collection-name");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    words.append(&title);
    let n = widgets::label(line, "collection-count");
    n.set_xalign(0.0);
    n.set_ellipsize(gtk::pango::EllipsizeMode::End);
    words.append(&n);
    row.append(&words);
    tile.set_child(Some(&row));
    tile.connect_clicked(move |_| action());
    tile
}

fn open_category(key: String) {
    *ui().category.borrow_mut() = key;
    ui().scroll.vadjustment().set_value(0.0);
    render();
}

/// The symbol for a NyxOS catalog section.
fn section_icon(section: &str) -> &'static str {
    match section {
        "cli" => "utilities-terminal-symbolic",
        "toolchains" => "applications-engineering-symbolic",
        "infrastructure" => "network-server-symbolic",
        "agents" => "system-run-symbolic",
        _ => "view-app-grid-symbolic",
    }
}

/// The NyxOS catalog's sections, in its order, as (id, name).
fn sections(c: &Catalog) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for g in &c.groups {
        if !out.contains(&g.section) {
            out.push(g.section.clone());
        }
    }
    out
}

/// Categories: the kinds of apps, then on NyxOS the catalog's other sections (terminal
/// tools, toolchains, infrastructure, agents), each with its categories.
fn render_categories(c: &Catalog) {
    let u = ui();
    let apps = browsable(c);
    heading("Categories", "Find the right app for what you want to do.");
    let nix = nyx_store::nix::active();
    let grid = tile_grid();
    for &(name, icon) in catalog::CATEGORIES {
        let count = apps.iter().filter(|a| a.desktop && a.category == name).count();
        grid.insert(&tile(name, &format!("{count} apps"), icon, theme::tone(name), move || open_category(name.into())), -1);
    }
    u.body.append(&grid);
    if !nix {
        return;
    }
    for (section, section_name) in sections(c).into_iter().filter(|(id, _)| id != "nyx-apps" && id != "desktop") {
        let groups: Vec<(&catalog::Group, usize)> = c
            .groups
            .iter()
            .filter(|g| g.section.0 == section)
            .map(|g| (g, c.apps.iter().filter(|a| a.group.0 == g.id && !a.built_in).count()))
            .filter(|(_, n)| *n > 0)
            .collect();
        if groups.is_empty() {
            continue;
        }
        heading(&section_name, "");
        let grid = tile_grid();
        for (g, count) in groups {
            let key = format!("catalog:{}", g.id);
            let noun = if c.apps.iter().any(|a| a.group.0 == g.id && a.desktop) { "apps" } else { "tools" };
            grid.insert(&tile(&g.name, &format!("{count} {noun}"), section_icon(&section), theme::tone(&section_name), move || open_category(key.clone())), -1);
        }
        u.body.append(&grid);
    }
}

/// One category's apps: a kind of app (`Graphics`), or a catalog category
/// (`catalog:coding-agents`).
fn render_category(c: &Catalog, key: &str) {
    let (title, mut apps): (String, Vec<&App>) = match key.strip_prefix("catalog:") {
        Some(id) => (
            c.groups.iter().find(|g| g.id == id).map(|g| g.name.clone()).unwrap_or_else(|| id.to_string()),
            c.apps.iter().filter(|a| a.group.0 == id && !a.built_in).collect(),
        ),
        None => (key.to_string(), browsable(c).into_iter().filter(|a| a.desktop && a.category == key).collect()),
    };
    if !key.starts_with("catalog:") {
        apps.sort_by_cached_key(|a| (a.source != nyx_store::nix::NYXOS && !a.popular, a.name.to_lowercase()));
    }
    heading(&title, "");
    if apps.is_empty() {
        text("Nothing here yet.");
        return;
    }
    let shown: Vec<App> = apps.iter().take(200).map(|a| (*a).clone()).collect();
    grid(&shown);
    if apps.len() > 200 {
        text("Showing the first 200 apps. Search to find more.");
    }
}

/// Nyx Apps: the Nyx suite, in the catalog's categories.
fn render_nyx(c: &Catalog) {
    let apps: Vec<&App> = c.apps.iter().filter(|a| a.nyx).collect();
    let have = apps.iter().filter(|a| a.installed.is_some()).count();
    heading(
        "Nyx Apps",
        &format!("Apps made for NyxOS, in one design and working together. {have} of {} installed.", apps.len()),
    );
    for g in &c.groups {
        let group: Vec<App> = apps.iter().filter(|a| a.group.0 == g.id).map(|a| (*a).clone()).collect();
        if group.is_empty() {
            continue;
        }
        heading(&g.name, "");
        grid(&group);
    }
}

/// Installed: on NyxOS everything installed, by the catalog's sections; on Arch the
/// desktop apps, each from the source it was installed from.
fn render_installed(c: &Catalog) {
    if nyx_store::nix::active() {
        heading("Your apps", "Everything installed on this computer, from the NyxOS catalog, nixpkgs and Flathub.");
        let installed: Vec<&App> = c.apps.iter().filter(|a| a.installed.is_some()).collect();
        let offered: BTreeSet<(String, String)> = installed.iter().flat_map(|a| a.also.iter().cloned()).collect();
        let mut seen = BTreeSet::new();
        let installed: Vec<&App> = installed
            .into_iter()
            .filter(|a| !offered.contains(&(a.source.clone(), a.package.clone())))
            .filter(|a| seen.insert((a.source.as_str(), a.package.as_str())))
            .collect();
        if installed.is_empty() {
            text("Nothing installed from Store yet.");
            return;
        }
        for (section, name) in sections(c) {
            let mut apps: Vec<App> = installed
                .iter()
                .filter(|a| a.section.0 == section || (section == "desktop" && a.section.0.is_empty()))
                .map(|a| (*a).clone())
                .collect();
            if apps.is_empty() {
                continue;
            }
            apps.sort_by_cached_key(|a| a.name.to_lowercase());
            heading(&name, "");
            grid(&apps);
        }
        return;
    }
    heading("Your apps", "Desktop apps installed on this computer.");
    let mut apps = Vec::new();
    let mut seen = BTreeSet::new();
    for a in c.apps.iter().filter(|a| a.installed.is_some() && a.desktop) {
        // The source it was installed from, when another has it too.
        if c.sources.get(&a.package).is_some_and(|(s, _)| s != &a.source)
            && c.apps.iter().any(|other| {
                other.package == a.package && c.sources.get(&a.package).is_some_and(|(s, _)| s == &other.source)
            })
        {
            continue;
        }
        if seen.insert(a.package.clone()) {
            apps.push(a.clone());
        }
    }
    if apps.is_empty() {
        text("No desktop apps installed from Store's sources yet.");
    } else {
        grid(&apps);
    }
}

/// Search: the results for the query, best first, with a button per source that has
/// any (and Include packages) above them. The source buttons only filter this page.
fn render_search(c: &Catalog) {
    let u = ui();
    let query = u.search.text().to_string();
    let filter = u.search_source.borrow().clone();
    let include = u.include_packages.get();
    let nix = nyx_store::nix::active();
    let aur = u.aur.borrow();
    let matched: Vec<&App> =
        c.apps.iter().chain(aur.iter()).filter(|a| nyx_store::search::score(a, &query).is_some()).collect();
    let shown = |a: &App, filter: Option<&str>| match filter {
        None => include || a.desktop || a.source == nyx_store::nix::NYXOS,
        Some("nyx") => a.nyx,
        Some(s) => {
            a.source == s && (include || a.desktop || matches!(s, "aur") || s == nyx_store::nix::NIXPKGS || s == nyx_store::nix::NYXOS)
        }
    };
    let results_for = |f: Option<&str>| nyx_store::search::rank(matched.iter().copied().filter(|a| shown(a, f)), &query, f.is_none());
    let results = results_for(filter.as_deref());

    let head = widgets::hbox(10);
    head.add_css_class("search-head");
    let title = widgets::label(&format!("Results for “{}”", query.trim()), "shelf-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    head.append(&title);
    let count = widgets::label(&format!("{}", results.len()), "collection-count");
    count.set_valign(gtk::Align::Center);
    head.append(&count);
    u.body.append(&head);

    let mut sources: Vec<(String, String)> = Vec::new();
    if nix {
        sources.push(("nyx".into(), "Nyx Apps".into()));
        for s in [nyx_store::nix::NYXOS, nyx_store::nix::FLATHUB, nyx_store::nix::NIXPKGS] {
            sources.push((s.into(), source_name(s).into()));
        }
    } else {
        for r in c.repositories.iter().filter(|r| r.enabled) {
            if !sources.iter().any(|(id, _)| id == &r.name) {
                sources.push((r.name.clone(), r.name.clone()));
            }
        }
        sources.push(("aur".into(), "AUR".into()));
    }
    let mut options = vec![(String::new(), format!("All  {}", results_for(None).len()))];
    for (id, name) in sources {
        let n = results_for(Some(&id)).len();
        if n > 0 || filter.as_deref() == Some(id.as_str()) {
            options.push((id, format!("{name}  {n}")));
        }
    }
    let filters = widgets::hbox(12);
    filters.add_css_class("search-filters");
    let buttons = widgets::segmented(&options, filter.as_deref().unwrap_or(""), |id| {
        *ui().search_source.borrow_mut() = (!id.is_empty()).then_some(id);
        glib::idle_add_local_once(render);
    });
    buttons.set_hexpand(true);
    buttons.set_halign(gtk::Align::Start);
    filters.append(&buttons);
    let packages = gtk::CheckButton::with_label("Include packages");
    packages.set_tooltip_text(Some("Include command-line tools, libraries and system packages in search"));
    packages.set_active(include);
    packages.set_valign(gtk::Align::Center);
    packages.connect_toggled(|b| {
        ui().include_packages.set(b.is_active());
        glib::idle_add_local_once(render);
    });
    filters.append(&packages);
    u.body.append(&filters);

    if results.is_empty() && !u.searching.get() {
        let hint = if filter.is_some() {
            "Try All."
        } else if !include {
            "Try Include packages to search libraries and command-line packages too."
        } else {
            "Try other words."
        };
        text(&format!("Nothing matches “{}”. {hint}", query.trim()));
    } else {
        let shown: Vec<App> = results.iter().take(200).map(|a| (*a).clone()).collect();
        grid(&shown);
        if results.len() > 200 {
            text("Showing the best 200 results. Add a word to narrow them down.");
        }
    }
    let remote = if nix { "nixpkgs" } else { "the AUR" };
    if u.searching.get() {
        let row = widgets::hbox(8);
        row.add_css_class("search-more");
        let spinner = gtk::Spinner::new();
        spinner.start();
        row.append(&spinner);
        row.append(&widgets::label(&format!("Searching {remote}…"), "collection-count"));
        u.body.append(&row);
    } else if !u.search_error.borrow().is_empty() {
        let row = widgets::label(&format!("Couldn't search {remote}: {}", u.search_error.borrow()), "collection-count");
        row.add_css_class("search-more");
        row.set_wrap(true);
        row.set_xalign(0.0);
        u.body.append(&row);
    }
}

/// Apps as compact rows in up to three columns: the icon, the name and one line about
/// it, and Install or Open at the end. A row opens the app's details.
fn grid(apps: &[App]) {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .min_children_per_line(grid_columns())
        .max_children_per_line(3)
        .column_spacing(16)
        .row_spacing(4)
        .homogeneous(true)
        .build();
    grid.add_css_class("store-grid");
    let page = ui().page.borrow().clone();
    let searching = page == "search";
    // Where sources mix, each row says where it's from.
    let tagged = nyx_store::nix::active() && matches!(page.as_str(), "search" | "categories" | "installed");
    let shown: Vec<App> = apps.iter().take(200).cloned().collect();
    for app in &shown {
        let row = widgets::hbox(12);
        row.add_css_class("app-row");
        // Narrow enough for two columns in a tiled half-screen window.
        row.set_size_request(250, -1);
        if !app.summary.is_empty() {
            row.set_tooltip_text(Some(&app.summary));
        }
        let icon = app_icon(app, 44);
        icon.set_valign(gtk::Align::Center);
        row.append(&icon);
        let text = widgets::vbox(2);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        // Capped natural widths, so long summaries don't cost the grid its columns.
        let name = widgets::label(&app.name, "app-name");
        name.set_xalign(0.0);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(22);
        text.append(&name);
        let line = widgets::hbox(6);
        let about = widgets::label(if app.summary.is_empty() { &app.category } else { &app.summary }, "app-line");
        about.set_xalign(0.0);
        about.set_ellipsize(gtk::pango::EllipsizeMode::End);
        about.set_max_width_chars(26);
        about.set_hexpand(true);
        line.append(&about);
        if let Some(tag) = source_tag(app).filter(|t| tagged || *t == "AUR") {
            line.append(&widgets::label(tag, "row-tag"));
        }
        if app.built_in {
            line.append(&widgets::label("Built in", "row-tag"));
        } else if searching && app.installed.is_some() {
            line.append(&widgets::label("Installed", "row-tag"));
        }
        text.append(&line);
        row.append(&text);
        row.append(&row_action(app));
        grid.insert(&row, -1);
    }
    grid.connect_child_activated(move |_, child| {
        if let Some(app) = shown.get(child.index() as usize) {
            details(app.clone());
        }
    });
    ui().body.append(&grid);
}

/// How many columns of app rows fit the page: three in a full window, two in a tiled
/// half-screen one. Set from the width, since the grid would go by its rows' natural
/// widths and give up columns to long summaries.
fn grid_columns() -> u32 {
    let u = ui();
    let width = if u.window.width() > 0 { u.window.width() } else { u.window.default_width() };
    let sidebar = if u.nav.has_css_class("compact") { 56 } else { 212 };
    let content = width - sidebar - 48;
    ((content + 16) / (250 + 16)).clamp(1, 3) as u32
}

/// What a row does directly: Install an app that isn't installed, Open one that is,
/// or a check for an installed app without a launcher.
fn row_action(app: &App) -> gtk::Widget {
    if app.installed.is_some() {
        if app.desktop_id.is_empty() {
            let done = gtk::Image::from_icon_name("object-select-symbolic");
            done.add_css_class("row-installed");
            done.set_tooltip_text(Some("Installed"));
            done.set_size_request(32, 32);
            return done.upcast();
        }
        let open = widgets::bar_button("media-playback-start-symbolic", &format!("Open {}", app.name));
        open.add_css_class("row-action");
        open.set_valign(gtk::Align::Center);
        let a = app.clone();
        open.connect_clicked(move |_| launch(&a));
        return open.upcast();
    }
    let tip = if app.source == "aur" { format!("Review the build of {}", app.name) } else { format!("Install {}", app.name) };
    let get = widgets::bar_button("folder-download-symbolic", &tip);
    get.add_css_class("row-action");
    get.add_css_class("get");
    get.set_valign(gtk::Align::Center);
    get.set_sensitive(!ui().busy.get());
    let a = app.clone();
    get.connect_clicked(move |_| install(&a));
    get.upcast()
}

/// Open an installed app from its launcher.
fn launch(app: &App) {
    let desktop = app.desktop_id.clone();
    if let Some(info) = gio::AppInfo::all().into_iter().find(|a| a.id().is_some_and(|id| id == desktop)) {
        if let Err(e) = info.launch(&[], None::<&gio::AppLaunchContext>) {
            notice(&e.to_string());
        }
    } else {
        notice("The app’s desktop launcher is unavailable.");
    }
}

/// Install an app: a repository app after the review of the package changes, an AUR
/// app after the review of its build files.
fn install(app: &App) {
    if app.source == "aur" {
        review_aur(&app.package);
    } else {
        start_request(Request::Install { source: app.source.clone(), packages: vec![app.package.clone()] });
    }
}

fn details(app: App) {
    let (dialog, card) = widgets::dialog(&app.name, 680);
    dialog.set_default_height(620);
    let content = widgets::vbox(14);
    let head = widgets::hbox(14);
    head.append(&app_icon(&app, 64));
    let info = widgets::vbox(6);
    let summary = widgets::label(&app.summary, "app-summary");
    summary.set_wrap(true);
    summary.set_xalign(0.0);
    info.append(&summary);
    let source = if app.version.is_empty() {
        source_name(&app.source).to_string()
    } else {
        format!("{} · {}", source_name(&app.source), app.version)
    };
    info.append(&widgets::label(&source, "source-tag"));
    head.append(&info);
    content.append(&head);
    let description = widgets::label(if app.description.is_empty() { &app.summary } else { &app.description }, "");
    description.set_wrap(true);
    description.set_xalign(0.0);
    description.set_selectable(true);
    // Focused, a selectable label selects all its text; it's still selected by dragging.
    description.set_focusable(false);
    content.append(&description);
    if app.source == "aur" {
        content.append(&widgets::banner(
            "AUR packages are community build recipes. Review the files before building. Builds run as your user.",
            true,
        ));
    }
    if !app.license.is_empty() {
        content.append(&widgets::row("License", &app.license, None));
    }
    content.append(&widgets::row("Package", &app.package, None));
    if nyx_store::nix::active() && !app.scope.is_empty() {
        let (title, detail) = if app.built_in {
            ("Built into NyxOS", "Part of the system for everyone; it can't be removed.")
        } else if app.scope == "everyone" {
            ("Installs for everyone", "Built into a new system generation; asks for an administrator's password.")
        } else {
            ("Installs for you", "Into your own apps; no password needed.")
        };
        content.append(&widgets::row(title, detail, None));
    }
    for (source, package) in &app.also {
        let other = ui().catalog.borrow().as_ref().and_then(|c| c.apps.iter().find(|a| &a.source == source && &a.package == package).cloned());
        let Some(other) = other else { continue };
        let d = dialog.clone();
        let (row, _) = widgets::button_row(
            &format!("Also on {}", source_name(source)),
            &if other.installed.is_some() { format!("{} · installed", other.name) } else { other.name.clone() },
            "Show",
            move |_| {
                d.close();
                details(other.clone());
            },
        );
        content.append(&row);
    }
    if nyx_store::nix::active() && !app.built_in && [nyx_store::nix::NYXOS, nyx_store::nix::NIXPKGS].contains(&app.source.as_str()) {
        let command = format!("nyx install {}", app.package);
        let copy = widgets::bar_button("edit-copy-symbolic", "Copy the command");
        copy.set_valign(gtk::Align::Center);
        let c = command.clone();
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&c);
            toast("Copied the install command");
        });
        let row = widgets::row("Install from a terminal", &command, Some(copy.upcast_ref()));
        row.add_css_class("install-command");
        content.append(&row);
    }
    if let Some(version) = &app.installed {
        content.append(&widgets::row("Installed version", version, None));
    }
    if !app.website.is_empty() && app.website.starts_with("https://") {
        let link = gtk::LinkButton::with_label(&app.website, "Website");
        link.set_halign(gtk::Align::Start);
        content.append(&link);
    }
    for index in 0..app.screenshots.len().min(3) {
        let picture = gtk::Picture::new();
        picture.set_can_shrink(true);
        picture.set_content_fit(gtk::ContentFit::Contain);
        picture.set_height_request(220);
        content.append(&picture);
        load_picture(&app, index, &picture);
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&content)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(280)
        .max_content_height(540)
        .propagate_natural_height(true)
        .build();
    card.append(&scroll);
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    let close = button("Close", move || d.close());
    actions.append(&close);
    if app.installed.is_some() {
        if !app.desktop_id.is_empty() {
            let a = app.clone();
            actions.append(&button("Open", move || launch(&a)));
        }
        if app.removable() {
            let package = app.package.clone();
            let d = dialog.clone();
            let armed = Rc::new(Cell::new(false));
            let remove = gtk::Button::with_label("Remove");
            remove.add_css_class("destructive-action");
            remove.connect_clicked(move |b| {
                if armed.replace(true) {
                    d.close();
                    start_request(Request::Remove { package: package.clone() });
                } else {
                    b.set_label("Click again to remove");
                    let a = armed.clone();
                    let b = b.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                        a.set(false);
                        b.set_label("Remove");
                    });
                }
            });
            actions.append(&remove);
        }
    } else {
        let d = dialog.clone();
        let a = app.clone();
        let get = button(if app.source == "aur" { "Review build" } else { "Install" }, move || {
            d.close();
            install(&a);
        });
        get.add_css_class("suggested-action");
        get.set_sensitive(!ui().busy.get());
        actions.append(&get);
    }
    card.append(&actions);
    dialog.present();
    close.grab_focus();
}

/// A source as people read it.
fn source_name(source: &str) -> &str {
    match source {
        "nyxos" => "NyxOS catalog",
        "nixpkgs" => "nixpkgs",
        "flathub" => "Flathub",
        other => other,
    }
}

fn render_updates(c: &Catalog) {
    if nyx_store::nix::active() {
        render_nixos_updates(c);
        return;
    }
    heading(
        "Repository updates",
        "Arch updates are coordinated across the system. Store verifies every package’s original source before installation.",
    );
    let unknown = c.installed.values().filter(|p| !c.sources.get(&p.name).is_some_and(|(_, s)| s == &p.stamp)).count();
    if unknown > 0 {
        let row = widgets::button_row(
            "Review existing sources",
            &format!(
                "{unknown} installed packages have no confirmed origin. Packages requiring an update will be blocked until reviewed."
            ),
            "Review",
            |_| source_review(),
        );
        ui().body.append(&row.0);
    }
    let update = button("Review system update", || start_request(Request::Update));
    update.add_css_class("suggested-action");
    update.set_halign(gtk::Align::Start);
    update.set_margin_top(12);
    update.set_sensitive(!ui().busy.get());
    ui().body.append(&update);
    if c.updates.is_empty() {
        text("No repository updates in the cached catalog. Refresh to check the latest versions.");
    } else {
        for change in &c.updates {
            let row = widgets::row(
                &change.name,
                &format!("{} → {} · {}", change.old.as_deref().unwrap_or(""), change.new.as_deref().unwrap_or(""), change.source),
                None,
            );
            ui().body.append(&row);
        }
    }
    heading("AUR updates", "Rebuild community packages from reviewed recipes. Repository dependencies are updated first.");
    let b = button("Check AUR updates", check_aur_updates);
    b.set_halign(gtk::Align::Start);
    ui().body.append(&b);
    let u = ui();
    if u.aur_updates.borrow().is_empty() {
        text("Check for updates to packages whose recorded source is AUR.");
    }
    for a in u.aur_updates.borrow().iter() {
        let name = a.package.clone();
        let row = widgets::button_row(
            &a.name,
            &format!("{} → {}", a.installed.as_deref().unwrap_or(""), a.version),
            "Review build",
            move |_| review_aur(&name),
        );
        u.body.append(&row.0);
    }
}

/// NyxOS: the system updates as one (NyxOS, its Nyx apps and everything installed for
/// everyone), Flathub's apps with it, and a system change can be undone a generation.
fn render_nixos_updates(c: &Catalog) {
    heading(
        "Updates",
        "NyxOS, its Nyx apps and the software installed for everyone update together into a new system generation, switched to only when it builds. Your Flathub apps update with them.",
    );
    let update = button("Update everything", || start_request(Request::Update));
    update.add_css_class("suggested-action");
    update.set_halign(gtk::Align::Start);
    update.set_margin_top(12);
    update.set_sensitive(!ui().busy.get());
    ui().body.append(&update);
    let checked = ui().system_updates.borrow().clone();
    if let Some((time, items)) = checked {
        if items.is_empty() {
            text(&format!("Everything was up to date at the last check, {}.", ago(time)));
        } else {
            let note = widgets::label(&format!("Found by the last check, {}.", ago(time)), "collection-note");
            note.set_xalign(0.0);
            note.set_margin_top(12);
            ui().body.append(&note);
            let list = widgets::vbox(0);
            list.add_css_class("group-list");
            for change in &items {
                let detail = match (&change.old, &change.new) {
                    (Some(old), Some(new)) => format!("{old} → {new} · {}", change.source),
                    (None, Some(new)) => format!("{new} · {}", change.source),
                    _ => change.source.clone(),
                };
                list.append(&widgets::row(&change.name, &detail, None));
            }
            ui().body.append(&list);
        }
    } else if c.updates.is_empty() {
        text("No Flathub updates in the cached catalog. Refresh to check again.");
    } else {
        for change in &c.updates {
            let row = widgets::row(&change.name, &format!("{} · Flathub", change.new.as_deref().unwrap_or("")), None);
            ui().body.append(&row);
        }
    }
    heading("System generations", "Every system change makes a new generation. Going back undoes the last one, software and settings with it; your files stay.");
    let (count, current, previous) = nyx_store::nix::generations();
    let detail = match current {
        Some(n) => format!("This is generation {n}; {count} are kept. Older ones are also in the boot menu."),
        None => "The current generation isn't known.".into(),
    };
    let (row, back) = widgets::button_row("Go back a generation", &detail, "Go Back", |_| start_request(Request::Rollback));
    back.set_sensitive(previous.is_some() && !ui().busy.get());
    ui().body.append(&row);
}

/// How long ago a time (seconds since the epoch) was, in words.
fn ago(time: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let plural = |n: i64, unit: &str| format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" });
    match (now - time).max(0) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => plural(s / 60, "minute"),
        s if s < 86400 => plural(s / 3600, "hour"),
        s => plural(s / 86400, "day"),
    }
}

fn check_aur_updates() {
    let u = ui();
    if u.busy.get() {
        return;
    }
    let names: Vec<String> = u
        .catalog
        .borrow()
        .as_ref()
        .map(|c| {
            c.installed
                .values()
                .filter(|p| c.sources.get(&p.name).is_some_and(|(s, stamp)| s == "aur" && stamp == &p.stamp))
                .map(|p| p.name.clone())
                .collect()
        })
        .unwrap_or_default();
    if names.is_empty() {
        toast("No packages have a confirmed AUR source.");
        return;
    }
    let (tx, rx) = async_channel::bounded(1);
    u.status.set_text("Checking AUR updates…");
    std::thread::spawn(move || {
        let _ = tx.send_blocking(aur::info(&names).map_err(|e| e.to_string()));
    });
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            match result {
                Ok(mut apps) => {
                    let u = ui();
                    if let Some(c) = u.catalog.borrow().as_ref() {
                        apps.retain_mut(|a| {
                            a.installed = c.installed.get(&a.package).map(|p| p.version.clone());
                            a.installed.as_ref().is_some_and(|old| alpm::vercmp(a.version.as_str(), old.as_str()).is_gt())
                        });
                    }
                    let count = apps.len();
                    *u.aur_updates.borrow_mut() = apps;
                    render();
                    toast(&format!("{count} AUR updates available"));
                }
                Err(e) => notice(&e),
            }
        }
    });
}

fn render_repositories(c: &Catalog) {
    heading("Included sources", "Omarchy always has first priority. Updates keep each app’s original source.");
    let included = widgets::vbox(0);
    included.add_css_class("group-list");
    for (title, description, status) in [
        ("Omarchy", "The preferred source for Omarchy apps and packages.", "First priority"),
        ("Arch Linux", "Core, extra and multilib repositories.", "Enabled"),
        ("AUR", "Community recipes, reviewed and built as your user.", "Available"),
    ] {
        let state = widgets::label(status, "source-tag");
        included.append(&widgets::row(title, description, Some(state.upcast_ref())));
    }
    ui().body.append(&included);
    heading("Additional sources", "Add more apps without changing your system’s repository order.");
    let additional = widgets::vbox(0);
    additional.add_css_class("group-list");
    let blackarch = c.repositories.iter().find(|r| r.name == "blackarch").cloned().unwrap_or_else(repositories::blackarch);
    let enabled = blackarch.enabled;
    let (row, action) = widgets::button_row(
        "BlackArch",
        if enabled { "Security and research tools · enabled" } else { "Security and research tools · sets up automatically" },
        if enabled { "Disable" } else { "Setup" },
        move |_| {
            if enabled {
                let mut repo = blackarch.clone();
                repo.enabled = false;
                start_request(Request::Repository { repository: repo });
            } else {
                start_request(Request::BlackarchSetup);
            }
        },
    );
    action.set_sensitive(!ui().busy.get());
    additional.append(&row);
    let system = repositories::system_config().ok();
    for repo in c.repositories.iter().filter(|r| !repositories::official(&r.name) && r.name != "blackarch") {
        if system.as_ref().is_some_and(|c| c.repos.iter().any(|r| r.name == repo.name)) {
            additional.append(&widgets::row(&repo.name, "Managed by your system configuration.", None));
        } else {
            let repo = repo.clone();
            let name = repo.name.clone();
            let row = widgets::button_row(&name, if repo.enabled { "Enabled" } else { "Disabled" }, "Manage", move |_| {
                repository_dialog(Some(repo.clone()))
            });
            additional.append(&row.0);
        }
    }
    ui().body.append(&additional);
    let add = button("Add repository", || repository_dialog(None));
    add.set_halign(gtk::Align::Start);
    add.set_margin_top(12);
    ui().body.append(&add);
    heading("Update protection", "Confirm the original sources of apps installed before Store.");
    let unknown = c.installed.values().filter(|p| !c.sources.get(&p.name).is_some_and(|(_, stamp)| stamp == &p.stamp)).count();
    let sources = widgets::vbox(0);
    sources.add_css_class("group-list");
    let row = widgets::button_row(
        "Existing installations",
        &format!("{unknown} packages need their original source confirmed."),
        "Review",
        |_| source_review(),
    );
    sources.append(&row.0);
    ui().body.append(&sources);
}

fn repository_dialog(repo: Option<repositories::Repository>) {
    let (dialog, card) = widgets::dialog(if repo.is_some() { "Manage repository" } else { "Add repository" }, 560);
    let repo = repo.unwrap_or(repositories::Repository { name: String::new(), servers: vec![String::new()], enabled: true });
    let name = gtk::Entry::new();
    name.set_placeholder_text(Some("Repository name"));
    name.set_text(&repo.name);
    name.set_editable(repo.name.is_empty());
    card.append(&name);
    let server = gtk::Entry::new();
    server.set_placeholder_text(Some("https://mirror.example/$repo/os/$arch"));
    server.set_text(repo.servers.first().map(String::as_str).unwrap_or(""));
    card.append(&server);
    let enabled = gtk::CheckButton::with_label("Enable this source");
    enabled.set_active(repo.enabled);
    card.append(&enabled);
    let label = widgets::label(
        "Signed packages are required. Trust the source’s full signing-key fingerprint separately. Disabling a source blocks updates for its installed packages.",
        "dim",
    );
    label.set_wrap(true);
    card.append(&label);
    let trust = button("Add signing key…", key_dialog);
    trust.set_halign(gtk::Align::Start);
    card.append(&trust);
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    actions.append(&button("Not now", move || d.close()));
    let d = dialog.clone();
    let all_servers = repo.servers.clone();
    actions.append(&button("Save", move || {
        let mut servers = all_servers.clone();
        if servers.is_empty() {
            servers.push(server.text().to_string());
        } else {
            servers[0] = server.text().to_string();
        }
        let repository = repositories::Repository { name: name.text().trim().to_string(), servers, enabled: enabled.is_active() };
        if let Err(e) = repositories::validate(&repository) {
            notice(&e.to_string());
            return;
        }
        d.close();
        start_request(Request::Repository { repository });
    }));
    card.append(&actions);
    dialog.present();
}
fn key_dialog() {
    widgets::ask_text(
        "Trust repository signing key",
        "Enter the complete 40-character fingerprint from the repository’s official website. Trusting this key allows its packages to run code as root when installed.",
        "",
        "Trust key",
        |fingerprint| start_request(Request::TrustKey { fingerprint: fingerprint.replace(' ', "").to_uppercase() }),
    );
}

fn source_review() {
    let u = ui();
    if u.busy.get() {
        return;
    }
    let c = u.catalog.borrow();
    let Some(c) = c.as_ref() else {
        return;
    };
    let mut groups: BTreeMap<Vec<String>, Vec<(String, String)>> = BTreeMap::new();
    for p in c.installed.values() {
        if c.sources.get(&p.name).is_some_and(|(_, s)| s == &p.stamp) {
            continue;
        }
        groups.entry(c.candidates.get(&p.name).cloned().unwrap_or_default()).or_default().push((p.name.clone(), p.stamp.clone()));
    }
    if groups.is_empty() {
        toast("Every installed package has a confirmed source.");
        return;
    }
    let (dialog, card) = widgets::dialog("Review existing sources", 720);
    dialog.set_default_height(680);
    let note = widgets::label(
        "Pacman does not retain a reliable original repository. These are suggestions, not proven history. Confirm only the groups whose source you recognize; unresolved packages stay protected from updates.",
        "dim",
    );
    note.set_wrap(true);
    card.append(&note);
    let content = widgets::vbox(16);
    let mut selections = Vec::new();
    for (candidates, packages) in groups {
        let group = widgets::vbox(8);
        let default = if candidates.len() == 1 { candidates[0].as_str() } else { "Choose source" };
        let check = gtk::CheckButton::with_label(&format!("Confirm {} packages · {default}", packages.len()));
        check.set_active(candidates.len() == 1);
        group.append(&check);
        let options: Vec<String> = if candidates.is_empty() {
            vec!["Leave unresolved".into(), "aur".into(), "local".into()]
        } else {
            let mut options = candidates.clone();
            options.extend(["aur".into(), "local".into()]);
            options
        };
        let refs: Vec<_> = options.iter().map(String::as_str).collect();
        let dd = gtk::DropDown::from_strings(&refs);
        group.append(&dd);
        let view = gtk::TextView::new();
        view.set_editable(false);
        view.set_cursor_visible(false);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.add_css_class("recipe");
        view.buffer().set_text(&packages.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
        let list = gtk::ScrolledWindow::builder()
            .child(&view)
            .min_content_height(70)
            .max_content_height(130)
            .propagate_natural_height(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .build();
        group.append(&list);
        content.append(&group);
        selections.push((check, dd, options, packages));
    }
    let scroll = gtk::ScrolledWindow::builder().child(&content).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    card.append(&scroll);
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    actions.append(&button("Not now", move || d.close()));
    let d = dialog.clone();
    actions.append(&button("Confirm selected sources", move || {
        let mut assignments = Vec::new();
        for (check, dd, options, packages) in &selections {
            if check.is_active()
                && let Some(source) = options.get(dd.selected() as usize).filter(|s| s.as_str() != "Leave unresolved") {
                    for (name, stamp) in packages {
                        assignments.push((name.clone(), source.clone(), stamp.clone()));
                    }
                }
        }
        if assignments.is_empty() {
            toast("Select at least one source group to confirm.");
            return;
        }
        d.close();
        start_request(Request::Sources { assignments });
    }));
    card.append(&actions);
    dialog.present();
}

fn start_request(request: Request) {
    let u = ui();
    if u.busy.get() {
        toast("A package operation is already running.");
        return;
    }
    let setup = matches!(request, Request::BlackarchSetup);
    if setup {
        blackarch_dialog::open();
    }
    let refresh_after = setup || matches!(request, Request::Repository { .. });
    busy(true);
    u.committed.set(setup);
    u.cancel_button.set_visible(!setup);
    *u.cancelled.borrow_mut() = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    notice("");
    u.status.set_text("Authorizing package operation…");
    let (send, receive) = async_channel::unbounded();
    let (confirm, confirmed) = mpsc::channel();
    *u.confirmation.borrow_mut() = Some(confirm);
    std::thread::spawn(move || transaction::start(request, send, confirmed));
    glib::spawn_future_local(async move {
        let mut done = false;
        while let Ok(event) = receive.recv().await {
            match event {
                Event::Status { message } => {
                    if setup {
                        blackarch_dialog::update(&message);
                    }
                    ui().status.set_text(message.trim());
                    ui().progress.pulse();
                }
                Event::Plan { plan } => {
                    if ui().cancelled.borrow().load(std::sync::atomic::Ordering::Relaxed) {
                        if let Some(tx) = ui().confirmation.borrow().as_ref() {
                            let _ = tx.send(None);
                        }
                    } else {
                        transaction_review(plan);
                    }
                }
                Event::Done { message } => {
                    done = true;
                    if setup {
                        blackarch_dialog::finish(&message, true);
                    }
                    operation_done(&message, refresh_after);
                    break;
                }
                Event::Error { message } => {
                    done = true;
                    if setup {
                        blackarch_dialog::finish(&message, false);
                    }
                    operation_error(&message);
                    break;
                }
            }
        }
        if !done {
            if setup {
                blackarch_dialog::finish("Setup was cancelled before it finished. You can retry.", false);
            }
            operation_error("Operation cancelled before installation.");
        }
    });
}
fn operation_error(message: &str) {
    let u = ui();
    u.committed.set(false);
    busy(false);
    u.confirmation.borrow_mut().take();
    u.queue.borrow_mut().clear();
    u.pending_recipe.borrow_mut().take();
    notice(message);
    u.status.set_text("Operation stopped");
}
fn operation_done(message: &str, refresh_after: bool) {
    let u = ui();
    busy(false);
    u.confirmation.borrow_mut().take();
    u.committed.set(false);
    toast(message);
    let next = u.queue.borrow_mut().pop_front();
    if let Some(next) = next {
        start_request(next);
        return;
    }
    let recipe = u.pending_recipe.borrow_mut().take();
    if let Some(recipe) = recipe {
        build_aur(recipe);
    } else {
        refresh(refresh_after);
    }
}
fn transaction_review(plan: Plan) {
    let (dialog, card) = widgets::dialog("Review package changes", 680);
    dialog.set_default_height(600);
    let bytes: i64 = plan.changes.iter().map(|c| c.bytes).sum();
    let note = widgets::label(
        &if nyx_store::nix::active() && plan.changes.iter().all(|c| c.source == "flathub") {
            format!(
                "{} {}. Flathub apps are yours alone: changing them needs no administrator's password.",
                plan.changes.len(),
                if plan.changes.len() == 1 { "change" } else { "changes" }
            )
        } else if nyx_store::nix::active() {
            format!(
                "{} {}. Software for everyone is built into a new system generation first, and only switched to when the build works. Applying asks for an administrator's password.",
                plan.changes.len(),
                if plan.changes.len() == 1 { "change" } else { "changes" }
            )
        } else {
            format!(
                "{} package changes · {:.1} MiB package data\nEvery update keeps its recorded source. Required system updates are included.",
                plan.changes.len(),
                bytes as f64 / 1048576.0
            )
        },
        "dim",
    );
    note.set_wrap(true);
    card.append(&note);
    let list = widgets::vbox(0);
    for c in &plan.changes {
        list.append(&widgets::row(
            &c.name,
            &format!(
                "{} → {} · {}",
                c.old.as_deref().filter(|o| !o.is_empty()).unwrap_or(if c.old.is_some() { "Installed" } else { "Not installed" }),
                c.new.as_deref().unwrap_or("Remove"),
                source_name(&c.source)
            ),
            None,
        ));
    }
    let scroll = gtk::ScrolledWindow::builder().child(&list).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build();
    card.append(&scroll);
    // What has to be agreed to (unfree licenses, services, groups): Apply waits for it.
    let agree = (!plan.notes.is_empty()).then(|| {
        for n in &plan.notes {
            card.append(&widgets::banner(&glib::markup_escape_text(n), true));
        }
        let check = gtk::CheckButton::with_label("I agree to the above");
        check.set_halign(gtk::Align::Start);
        card.append(&check);
        check
    });
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    actions.append(&button("Not now", move || d.close()));
    let approved = Rc::new(Cell::new(false));
    let a = approved.clone();
    let d = dialog.clone();
    let install = button("Apply changes", move || {
        a.set(true);
        ui().committed.set(true);
        ui().cancel_button.set_visible(false);
        if let Some(tx) = ui().confirmation.borrow().as_ref() {
            let _ = tx.send(Some(plan.clone()));
        }
        d.close();
    });
    install.add_css_class("suggested-action");
    if let Some(check) = &agree {
        install.set_sensitive(false);
        let i = install.clone();
        check.connect_toggled(move |c| i.set_sensitive(c.is_active()));
    }
    actions.append(&install);
    card.append(&actions);
    dialog.connect_close_request(move |_| {
        if !approved.get()
            && let Some(tx) = ui().confirmation.borrow().as_ref() {
                let _ = tx.send(None);
            }
        glib::Propagation::Proceed
    });
    dialog.present();
}

fn review_aur(package: &str) {
    if ui().busy.get() {
        return;
    }
    busy(true);
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    *ui().cancelled.borrow_mut() = cancel.clone();
    ui().status.set_text("Downloading the AUR recipe…");
    let package = package.to_string();
    let cache = nyx_core::paths::cache_dir();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = aur::recipe(&cache, &package)
            .and_then(|r| {
                let deps = aur::missing_dependencies(&r, &cache)?;
                Ok((r, deps))
            })
            .map_err(|e| e.to_string());
        let _ = tx.send_blocking(result);
    });
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await {
            busy(false);
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                toast("AUR recipe download cancelled.");
                return;
            }
            match result {
                Err(e) => notice(&e),
                Ok((recipe, dependencies)) => {
                    let (dialog, card) = widgets::dialog("Review AUR build files", 800);
                    dialog.set_default_height(700);
                    let note = widgets::label(
                        &format!(
                            "{} · {}\n{} repository build dependencies will be reviewed before building.",
                            recipe.base,
                            &recipe.commit[..12],
                            dependencies.len()
                        ),
                        "dim",
                    );
                    note.set_wrap(true);
                    card.append(&note);
                    let view = gtk::TextView::new();
                    view.set_editable(false);
                    view.set_cursor_visible(false);
                    view.set_wrap_mode(gtk::WrapMode::WordChar);
                    view.add_css_class("recipe");
                    view.buffer().set_text(&recipe.review);
                    let scroll = gtk::ScrolledWindow::builder()
                        .child(&view)
                        .vexpand(true)
                        .hscrollbar_policy(gtk::PolicyType::Never)
                        .build();
                    card.append(&scroll);
                    let actions = widgets::hbox(8);
                    actions.set_halign(gtk::Align::End);
                    let d = dialog.clone();
                    actions.append(&button("Not now", move || d.close()));
                    let d = dialog.clone();
                    actions.append(&button("Build reviewed recipe", move || {
                        d.close();
                        let u = ui();
                        *u.pending_recipe.borrow_mut() = Some(recipe.clone());
                        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
                        for (source, name) in &dependencies {
                            groups.entry(source.clone()).or_default().push(name.clone());
                        }
                        let requests: VecDeque<_> =
                            groups.into_iter().map(|(source, packages)| Request::Install { source, packages }).collect();
                        *u.queue.borrow_mut() = requests;
                        let next = u.queue.borrow_mut().pop_front();
                        if let Some(next) = next {
                            start_request(next);
                        } else {
                            u.pending_recipe.borrow_mut().take();
                            build_aur(recipe.clone());
                        }
                    }));
                    card.append(&actions);
                    dialog.present();
                }
            }
        }
    });
}
fn build_aur(recipe: Recipe) {
    busy(true);
    ui().committed.set(false);
    ui().cancel_button.set_visible(true);
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    *ui().cancelled.borrow_mut() = cancel.clone();
    ui().status.set_text("Building AUR package as your user…");
    let (tx, rx) = async_channel::unbounded();
    let r = recipe.clone();
    std::thread::spawn(move || {
        let result = aur::build(&r, &cancel, |message| {
            let _ = tx.send_blocking(Err(message));
        })
        .map_err(|e| e.to_string());
        let _ = tx.send_blocking(Ok(result));
    });
    glib::spawn_future_local(async move {
        while let Ok(event) = rx.recv().await {
            match event {
                Err(message) => ui().status.set_text(&message),
                Ok(result) => {
                    busy(false);
                    match result {
                        Ok(artifacts) => {
                            start_request(Request::AurInstall { artifacts, base: recipe.base, commit: recipe.commit })
                        }
                        Err(e) => operation_error(&e),
                    }
                    break;
                }
            }
        }
    });
}
fn cancel_operation() {
    let u = ui();
    if u.committed.get() {
        return;
    }
    u.cancelled.borrow().store(true, std::sync::atomic::Ordering::Relaxed);
    if let Some(tx) = u.confirmation.borrow().as_ref() {
        let _ = tx.send(None);
    }
    u.cancel_button.set_visible(false);
    u.status.set_text("Cancelling before installation…");
}

fn settings() {
    let u = ui();
    nyx_ui::settings_card::open(
        &u.overlay,
        &u.window,
        nyx_ui::settings_card::Spec {
            groups: &[("Catalog", "view-grid-symbolic", "AUR results, automatic refresh")],
            search_text: None,
            standard: Some(nyx_ui::settings_card::Standard {
                app: "store",
                about: nyx_ui::settings_card::About {
                    name: "Store",
                    icon: "io.github.design_nexus.nyx.Store",
                    version: env!("CARGO_PKG_VERSION"),
                    description: "Find and install apps, keeping track of where each came from.",
                    licence: env!("CARGO_PKG_LICENSE"),
                    website: "https://github.com/design-nexus/nyx-store",
                    issues: "https://github.com/design-nexus/nyx-store/issues",
                    credits: &[],
                },
                shortcuts: SHORTCUTS,
                reduce_motion: || prefs::get().reduce_motion,
                set_reduce_motion: |on| prefs::update(|p| p.reduce_motion = on),
            }),
        },
        || {
            let page = widgets::page("settings");
            let wrapper = widgets::vbox(0);
            wrapper.add_css_class("settings-group");
            wrapper.set_widget_name("Catalog");
            wrapper.append(&widgets::label("CATALOG", "group-title"));
            let group = widgets::vbox(0);
            group.add_css_class("group-list");
            wrapper.append(&group);
            page.body.append(&wrapper);
            let p = prefs::get();
            let (title, detail) = if nyx_store::nix::active() {
                ("Search all of nixpkgs", "Find packages beyond the apps in the catalog.")
            } else {
                ("Include AUR results", "Search community packages as well as official repositories.")
            };
            let (row, _) = widgets::switch_row(title, detail, p.include_aur, |on| {
                prefs::update(|p| p.include_aur = on);
                ui().search_generation.set(ui().search_generation.get() + 1);
                ui().aur.borrow_mut().clear();
                render();
            });
            group.append(&row);
            let (row, _) = widgets::choice_row("Refresh the catalog", "Manual refresh is always available with Ctrl+R.",
                vec![("startup".into(), "On start".into()), ("daily".into(), "Daily".into()), ("manual".into(), "Manually".into())],
                &p.catalog_refresh, |id| prefs::update(|p| p.catalog_refresh = id));
            group.append(&row);
            (page.root.upcast(), page.body)
        },
    );
}
fn shortcuts() {
    let (dialog, card) = widgets::dialog("Keyboard shortcuts", 520);
    let rows = widgets::vbox(0);
    rows.add_css_class("group-list");
    for (keys, action) in SHORTCUTS {
        rows.append(&widgets::row(action, "", Some(widgets::key_caps(keys).upcast_ref())));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&rows)
        .max_content_height(560)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .build();
    card.append(&scroll);
    let d = dialog.clone();
    let b = button("Close", move || d.close());
    b.set_halign(gtk::Align::End);
    card.append(&b);
    dialog.present();
}

/// A small rounded square in `tone` (a swatch name, or `accent`) with `icon` on it.
fn tone_chip(icon: &str, tone: &str, size: i32) -> gtk::Box {
    let chip = widgets::hbox(0);
    chip.add_css_class("tone-chip");
    chip.add_css_class(&format!("tone-{tone}"));
    chip.set_size_request(size, size);
    chip.set_hexpand(false);
    chip.set_halign(gtk::Align::Start);
    chip.set_valign(gtk::Align::Center);
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(size / 2);
    image.set_size_request(size, size);
    chip.append(&image);
    chip
}

/// The app's icon at `size` px. An app without one shows its category's symbol on a
/// tile in the category's colour.
/// The icon of the launcher with this desktop ID, from every launcher's read once.
fn launcher_icon(desktop_id: &str) -> Option<gio::Icon> {
    thread_local! {
        static ICONS: std::collections::HashMap<String, gio::Icon> = gio::AppInfo::all()
            .into_iter()
            .filter_map(|a| Some((a.id()?.to_string(), a.icon()?)))
            .collect();
    }
    ICONS.with(|icons| icons.get(desktop_id).cloned())
}

fn app_icon(app: &App, size: i32) -> gtk::Widget {
    let cached = app.icon.starts_with("@cache:");
    let display = gdk::Display::default().unwrap();
    let own = !app.icon.is_empty() && !cached && gtk::IconTheme::for_display(&display).has_icon(&app.icon);
    let symbol = catalog::CATEGORIES
        .iter()
        .find(|(category, _)| *category == app.category)
        .map(|(_, icon)| *icon)
        .unwrap_or("application-x-executable-symbolic");
    // An installed app the catalog has no icon for (the Nyx apps): its launcher's.
    if !own
        && !cached
        && !app.desktop_id.is_empty()
        && let Some(icon) = launcher_icon(&app.desktop_id)
    {
        let image = gtk::Image::from_gicon(&icon);
        image.set_pixel_size(size);
        return image.upcast();
    }
    if !own && !cached {
        let image = gtk::Image::from_icon_name(symbol);
        image.set_pixel_size(size / 2);
        image.set_size_request(size, size);
        let tile = widgets::hbox(0);
        tile.add_css_class("icon-tile");
        tile.add_css_class(&format!("tone-{}", theme::tone(&app.category)));
        tile.set_size_request(size, size);
        tile.set_hexpand(false);
        tile.set_halign(gtk::Align::Center);
        tile.append(&image);
        return tile.upcast();
    }
    let image = gtk::Image::from_icon_name(if own { &app.icon } else { symbol });
    image.set_pixel_size(size);
    if cached {
        let cache = nyx_core::paths::cache_dir();
        let icon = app.icon.clone();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(nyx_store::assets::icon(&cache, &icon));
        });
        let weak = image.downgrade();
        glib::spawn_future_local(async move {
            if let Ok(Ok(path)) = rx.recv().await
                && let Some(image) = weak.upgrade() {
                    let paintable = gtk::IconPaintable::for_file(
                        &gio::File::for_path(path),
                        image.pixel_size().max(16),
                        image.scale_factor(),
                    );
                    image.set_paintable(Some(&paintable));
                }
        });
    }
    image.upcast()
}

fn load_picture(app: &App, index: usize, picture: &gtk::Picture) {
    let app = app.clone();
    let cache = nyx_core::paths::cache_dir();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(nyx_store::assets::screenshot(&cache, &app, index));
    });
    let weak = picture.downgrade();
    glib::spawn_future_local(async move {
        if let Ok(result) = rx.recv().await
            && let Some(picture) = weak.upgrade() {
                match result {
                    Ok(path) => match gdk::Texture::from_filename(path) {
                        Ok(texture) => {
                            picture.set_paintable(Some(&texture));
                            picture.set_visible(true);
                        }
                        Err(_) => picture.set_visible(false),
                    },
                    Err(error) => {
                        picture.set_tooltip_text(Some(&error.to_string()));
                        picture.set_visible(false);
                    }
                }
            }
    });
}

/// The search field's words: AUR on Arch, nixpkgs on NixOS.
fn search_label() -> &'static str {
    if nyx_store::nix::active() { "Search apps and nixpkgs" } else { "Search apps and AUR" }
}
