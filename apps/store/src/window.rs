#[path = "app_page.rs"]
mod app_page;
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
    ("nyx", "Nyx apps", ""),
    ("categories", "All categories", "view-grid-symbolic"),
    ("installed", "Installed", "emblem-default-symbolic"),
    ("updates", "Updates", "software-update-available-symbolic"),
    ("repositories", "Sources", "network-server-symbolic"),
];
/// The kinds of app the sidebar lists under Categories, as (name, catalog category, icon).
const KINDS: &[(&str, &str, &str)] = &[
    ("Create", "Graphics", "applications-graphics-symbolic"),
    ("Work", "Productivity", "document-edit-symbolic"),
    ("Play", "Games", "input-gaming-symbolic"),
    ("Develop", "Development", "utilities-terminal-symbolic"),
];
/// Window widths where the layout changes: the icon rail below the first, the
/// spotlight's screenshot from the second.
const RAIL_WIDTH: i32 = nyx_ui::sidebar::NARROW_WIDTH;
const WIDE_WIDTH: i32 = 1180;
pub const SHORTCUTS: &[(&[&str], &str)] = &[
    (&["Ctrl", "F"], "Search apps"),
    (&["Ctrl", "B"], "Collapse sidebar"),
    (&["Alt", "Left"], "Back from an app, or to Home"),
    (&["Ctrl", "R"], "Refresh catalog"),
    (&["Ctrl", ","], "Settings"),
    (&["Ctrl", "Shift", "P"], "Command palette"),
    (&["Esc"], "Close an app, a panel or the search"),
    (&["F1"], "Keyboard shortcuts"),
    (&["Ctrl", "Q"], "Close Store"),
];

struct Ui {
    window: gtk::ApplicationWindow,
    overlay: gtk::Overlay,
    nav: gtk::Box,
    /// The header's app name, hidden beside the icon rail.
    header_title: Option<gtk::Widget>,
    nav_items: Vec<(String, nyx_ui::sidebar::NavItem)>,
    /// Under the sidebar: how many apps the catalog has and when it was checked.
    foot: gtk::Label,
    body: gtk::Box,
    scroll: gtk::ScrolledWindow,
    search: gtk::SearchEntry,
    search_hint: gtk::Label,
    /// The app whose page is open over the page it was opened from, and where that
    /// page was scrolled to.
    detail: RefCell<Option<App>>,
    detail_scroll: Cell<f64>,
    /// The layout for the window's width: 0 beside the icon rail, 1 without the
    /// spotlight's screenshot, 2 with it.
    tier: Cell<u8>,
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
    /// A package from `--app`, until the catalog has loaded.
    pending_app: RefCell<Option<String>>,
}
thread_local! { static UI: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) }; }
fn ui() -> Rc<Ui> {
    UI.with(|u| u.borrow().as_ref().unwrap().clone())
}

pub fn present(
    application: &gtk::Application,
    section: Option<&str>,
    query: Option<&str>,
    remove: Option<&str>,
    app: Option<&str>,
) {
    if UI.with(|u| u.borrow().is_none()) {
        build(application);
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
    } else if let Some(package) = app {
        *u.pending_app.borrow_mut() = Some(package.to_string());
        if u.catalog.borrow().is_some() && !u.loading.get() {
            offer_app();
        }
    }
}

/// `--app`: the page of the catalog's app with this package name.
fn offer_app() {
    let u = ui();
    let Some(package) = u.pending_app.borrow_mut().take() else { return };
    let found = u.catalog.borrow().as_ref().and_then(|c| browsable(c).into_iter().find(|a| a.package == package).cloned());
    match found {
        Some(app) => details(app),
        None => notice(&format!("The catalog has no app called {package}.")),
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
    let nav = widgets::vbox(0);
    nav.add_css_class("settings-navigation");
    nav.add_css_class("sidebar");
    nav.set_hexpand(false);
    let list = widgets::vbox(1);
    list.set_hexpand(false);
    let mut nav_items = Vec::new();
    let mut entry = |id: &str, icon: &str, title: &str| {
        let item = nyx_ui::sidebar::nav_item(if icon.is_empty() { "view-app-grid-symbolic" } else { icon }, title, title);
        if icon.is_empty()
            && let Some(row) = item.button.child().and_downcast::<gtk::Box>()
            && let Some(image) = row.first_child()
        {
            row.remove(&image);
            row.prepend(&widgets::logo(16));
        }
        let target = id.to_string();
        item.button.connect_clicked(move |_| navigate(&target));
        list.append(&item.button);
        nav_items.push((id.to_string(), item));
    };
    let page = |id: &str| PAGES.iter().find(|p| p.0 == id).map(|p| (p.2, p.1)).unwrap_or_default();
    for id in ["home", "nyx"].into_iter().filter(|id| page_available(id)) {
        let (icon, title) = page(id);
        entry(id, icon, title);
    }
    nav_group(&list, "Categories", nyx_ui::sidebar::ACCENT_SECTION);
    for &(name, category, icon) in KINDS {
        entry(&format!("cat:{category}"), icon, name);
    }
    let (icon, title) = page("categories");
    entry("categories", icon, title);
    nav_group(&list, "This computer", "swatch-green");
    for id in ["installed", "updates", "repositories"].into_iter().filter(|id| page_available(id)) {
        let (icon, title) = page(id);
        entry(id, icon, title);
    }
    // The catalog's status; clicking it refreshes the catalog.
    let foot_row = gtk::Button::new();
    foot_row.add_css_class("store-foot");
    foot_row.add_css_class("compact-hide");
    foot_row.set_tooltip_text(Some("Refresh catalog (Ctrl+R)"));
    foot_row.connect_clicked(|_| refresh(true));
    let foot_content = widgets::hbox(6);
    let checked = gtk::Image::from_icon_name("emblem-default-symbolic");
    checked.add_css_class("foot-ok");
    foot_content.append(&checked);
    let foot = widgets::label("Loading apps…", "foot-text");
    foot.set_ellipsize(gtk::pango::EllipsizeMode::End);
    foot_content.append(&foot);
    foot_row.set_child(Some(&foot_content));
    list.append(&foot_row);
    let nav_scroll = gtk::ScrolledWindow::builder()
        .hexpand(false)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .child(&list)
        .build();
    nav.append(&nav_scroll);
    let column = widgets::vbox(0);
    column.add_css_class("settings-content");
    column.set_hexpand(true);
    let search = gtk::SearchEntry::new();
    search.set_placeholder_text(Some(search_label()));
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
        .child(&nyx_ui::clamp::Clamp::new(&body, 1000))
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::External)
        .build();
    column.append(&scroll);

    // ----- Header -----
    // The app and the sidebar toggle; the search in the middle; the operation under
    // way and Settings at the end.
    let header = chrome::app_header(&window, "system-software-install-symbolic", "Store");
    let collapse = widgets::bar_button("sidebar-show-symbolic", "Collapse the sidebar (Ctrl+B)");
    collapse.connect_clicked(|_| toggle_sidebar());
    header.start.append(&collapse);
    let search_box = gtk::Overlay::new();
    search_box.set_child(Some(&search));
    let search_hint = widgets::label("Ctrl F", "search-key");
    search_hint.set_halign(gtk::Align::End);
    search_hint.set_valign(gtk::Align::Center);
    search_hint.set_margin_end(10);
    search_hint.set_can_target(false);
    search_box.add_overlay(&search_hint);
    header.center.append(&search_box);
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
    header.end.append(&chrome::settings_button(settings));
    let header_title = find_class(header.root.upcast_ref(), "app-title");
    let frame = nyx_ui::frame::AppFrame::new(&header.root);
    frame.root.add_css_class("store-frame");
    frame.set_sidebar(&nav);
    frame.set_page(&column);
    let overlay = frame.install(&window);
    let toast_label = widgets::label("", "store-toast");
    toast_label.add_css_class("toast");
    toast_label.set_wrap(true);
    toast_label.set_halign(gtk::Align::Center);
    toast_label.set_valign(gtk::Align::End);
    toast_label.set_margin_bottom(48);
    toast_label.set_visible(false);
    overlay.add_overlay(&toast_label);
    let p = prefs::get();
    let first = nyx_theme::start_page("home", &p.last_section);
    let (first, first_category) = match first.strip_prefix("cat:") {
        Some(category) => ("categories".to_string(), category.to_string()),
        None => (first, String::new()),
    };
    let u = Rc::new(Ui {
        window: window.clone(),
        overlay: overlay.clone(),
        nav: nav.clone(),
        header_title,
        nav_items,
        foot,
        body,
        scroll,
        search: search.clone(),
        search_hint,
        detail: RefCell::new(None),
        detail_scroll: Cell::new(0.0),
        tier: Cell::new(u8::MAX),
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
        category: RefCell::new(first_category),
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
        pending_app: RefCell::new(None),
    });
    UI.with(|v| *v.borrow_mut() = Some(u.clone()));
    search.connect_search_changed(|entry| {
        let u = ui();
        let page = u.page.borrow().clone();
        u.search_hint.set_visible(entry.text().is_empty() && u.tier.get() > 0);
        if !entry.text().is_empty() {
            u.detail.borrow_mut().take();
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
            gdk::Key::Left if mods.contains(gdk::ModifierType::ALT_MASK) => {
                if ui().detail.borrow().is_some() {
                    close_details();
                } else {
                    navigate("home");
                }
            }
            gdk::Key::Escape => {
                if nyx_ui::settings_card::is_open() {
                    nyx_ui::settings_card::escape();
                } else if ui().detail.borrow().is_some() {
                    close_details();
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
    let narrow = width > 0 && width < RAIL_WIDTH;
    let compact = narrow || prefs::get().sidebar_collapsed;
    nyx_ui::settings_card::fit(&u.window);
    if compact != u.nav.has_css_class("compact") || !u.nav.has_css_class("sized") {
        u.nav.add_css_class("sized");
        nyx_ui::sidebar::set_compact(&u.nav, compact);
        fn rail_only(widget: &gtk::Widget, compact: bool) {
            if widget.has_css_class("compact-show") {
                widget.set_visible(compact);
            }
            let mut child = widget.first_child();
            while let Some(c) = child {
                rail_only(&c, compact);
                child = c.next_sibling();
            }
        }
        rail_only(u.nav.upcast_ref(), compact);
    }
    if narrow {
        u.body.add_css_class("narrow");
    } else {
        u.body.remove_css_class("narrow");
    }
    if let Some(title) = &u.header_title {
        title.set_visible(!narrow);
    }
    u.search.set_width_chars(if narrow { SEARCH_NARROW } else { SEARCH_WIDE });
    show_status();
    let tier = if narrow { 0 } else if width < WIDE_WIDTH { 1 } else { 2 };
    u.search_hint.set_visible(tier > 0 && u.search.text().is_empty());
    if u.tier.replace(tier) != tier {
        render();
        return;
    }
    let columns = grid_columns();
    let mut child = u.body.first_child();
    while let Some(c) = child {
        if let Some(grid) = c.downcast_ref::<gtk::FlowBox>().filter(|g| g.has_css_class("store-grid")) {
            grid.set_min_children_per_line(columns);
        }
        child = c.next_sibling();
    }
}
/// The header's status shows only while the catalog loads or an operation runs.
fn show_status() {
    let u = ui();
    u.status.set_visible(u.busy.get() || u.loading.get());
}
/// The first widget under `root` with a style class.
fn find_class(root: &gtk::Widget, class: &str) -> Option<gtk::Widget> {
    let mut child = root.first_child();
    while let Some(c) = child {
        if c.has_css_class(class) {
            return Some(c);
        }
        if let Some(found) = find_class(&c, class) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}
/// A sidebar group's heading in its section's colour, and the icon rail's bar in the
/// same colour.
fn nav_group(list: &gtk::Box, title: &str, section: &str) {
    let bar = gtk::Separator::new(gtk::Orientation::Horizontal);
    bar.add_css_class("nav-divider");
    bar.add_css_class("compact-show");
    bar.add_css_class(&format!("section-{section}"));
    bar.set_visible(false);
    list.append(&bar);
    let row = nyx_ui::sidebar::group_label(title, false);
    nyx_ui::sidebar::set_section(&row, section);
    list.append(&row);
}
/// Repositories is Arch's (NyxOS's sources need no managing).
fn page_available(page: &str) -> bool {
    match page {
        "repositories" => !nyx_store::nix::active(),
        _ => true,
    }
}
/// Go to a sidebar item: a page, or `cat:<category>` for one kind of app.
fn navigate(id: &str) {
    if id == "settings" {
        settings();
        return;
    }
    let (page, category) = match id.strip_prefix("cat:") {
        Some(category) => ("categories", category),
        None if id == "sources" => ("repositories", ""),
        None => (id, ""),
    };
    let u = ui();
    u.detail.borrow_mut().take();
    *u.page.borrow_mut() = page.into();
    *u.category.borrow_mut() = category.into();
    if page != "search" {
        u.search.set_text("");
        u.search_source.borrow_mut().take();
        *u.page.borrow_mut() = page.into();
    }
    prefs::update(|p| p.last_section = id.into());
    u.scroll.vadjustment().set_value(0.0);
    render();
}
/// Open an app's page over the page it's opened from.
fn details(app: App) {
    let u = ui();
    if u.detail.borrow().is_none() {
        u.detail_scroll.set(u.scroll.vadjustment().value());
    }
    *u.detail.borrow_mut() = Some(app);
    render();
    u.scroll.vadjustment().set_value(0.0);
}
/// Back from an app's page to where it was opened, scrolled as it was.
fn close_details() {
    let u = ui();
    if u.detail.borrow_mut().take().is_none() {
        return;
    }
    render();
    let y = u.detail_scroll.get();
    glib::idle_add_local_once(move || ui().scroll.vadjustment().set_value(y));
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
    show_status();
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
    show_status();
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
            show_status();
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
                        offer_app();
                    }
                }
                Err(e) => {
                    u.foot.set_text("Catalog unavailable");
                    notice(&e);
                }
            }
        }
    });
}

fn heading(title: &str, note: &str) {
    let u = ui();
    let g = widgets::vbox(2);
    g.add_css_class("store-heading");
    let l = widgets::label(title, "heading-title");
    l.set_wrap(true);
    g.append(&l);
    if !note.is_empty() {
        let n = widgets::label(note, "heading-note");
        n.set_wrap(true);
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
    let category = u.category.borrow().clone();
    let detail = u.detail.borrow().clone();
    let active = active_nav(&page, &category);
    for (id, item) in &u.nav_items {
        item.set_active(id == &active);
    }
    let c = u.catalog.borrow();
    let title = match &detail {
        Some(app) => app.name.clone(),
        None => place_name(c.as_ref(), &page, &category),
    };
    chrome::set_title(&u.window, "Store", &title);
    let Some(c) = c.as_ref() else {
        text("Loading your app catalog…");
        return;
    };
    update_badges(c);
    update_foot(c);
    if let Some(app) = detail {
        app_page::render(c, &current(c, app));
        return;
    }
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
}

/// An app as the catalog has it now (installed or removed since its page opened).
fn current(c: &Catalog, app: App) -> App {
    if let Some(now) = c.apps.iter().find(|a| a.source == app.source && a.package == app.package) {
        return now.clone();
    }
    App { installed: c.installed.get(&app.package).map(|p| p.version.clone()), ..app }
}

/// The sidebar item for a page: one of the four kinds, All categories for the others.
fn active_nav(page: &str, category: &str) -> String {
    match page {
        "categories" if KINDS.iter().any(|k| k.1 == category) => format!("cat:{category}"),
        "categories" if category == "essentials" => "home".into(),
        _ => page.into(),
    }
}

/// A page's name, for the window title and an app page's way back.
fn place_name(c: Option<&Catalog>, page: &str, category: &str) -> String {
    match page {
        "search" => "Results".into(),
        "categories" if !category.is_empty() => {
            if let Some(kind) = KINDS.iter().find(|k| k.1 == category) {
                return kind.0.into();
            }
            if category == "essentials" {
                return "Everyday essentials".into();
            }
            match category.strip_prefix("catalog:") {
                Some(id) => c.and_then(|c| c.groups.iter().find(|g| g.id == id)).map(|g| g.name.clone()).unwrap_or_else(|| id.into()),
                None => category.into(),
            }
        }
        _ => PAGES.iter().find(|p| p.0 == page).map(|p| p.1).unwrap_or("Home").into(),
    }
}

/// The counts beside the sidebar's items: the Nyx apps, each kind of app and what's
/// installed, and the updates waiting as a badge: NyxOS's from `nyx update`'s last
/// check (the Flathub ones in the catalog when there's none), Arch's from the catalog
/// and the AUR check.
fn update_badges(c: &Catalog) {
    let u = ui();
    let updates = if nyx_store::nix::active() {
        u.system_updates.borrow().as_ref().map(|(_, items)| items.len()).unwrap_or(c.updates.len())
    } else {
        c.updates.len() + u.aur_updates.borrow().len()
    };
    let apps = browsable(c);
    for (id, item) in &u.nav_items {
        let count = match id.as_str() {
            "nyx" => c.apps.iter().filter(|a| a.nyx).count(),
            "installed" => installed_list(c).len(),
            "updates" => {
                item.set_badge(Some(updates as u64));
                continue;
            }
            id => match id.strip_prefix("cat:") {
                Some(category) => apps.iter().filter(|a| a.desktop && a.category == category).count(),
                None => 0,
            },
        };
        item.set_count(Some(count as u64));
    }
}

/// Under the sidebar: the catalog's apps and when it was last refreshed.
fn update_foot(c: &Catalog) {
    let apps = c.apps.iter().filter(|a| a.desktop).map(|a| &a.package).collect::<BTreeSet<_>>().len();
    let checked = nyx_core::paths::cache_dir()
        .join("catalog-refreshed")
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| format!(" · checked {}", ago_short(d.as_secs() as i64)));
    ui().foot.set_text(&format!("{} apps{}", thousands(apps), checked.unwrap_or_default()));
}

/// 1148 as "1,148".
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
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
        None if key == "essentials" => {
            ("Everyday essentials".into(), browsable(c).into_iter().filter(|a| a.desktop && a.popular).collect())
        }
        None => (place_name(Some(c), "categories", key), browsable(c).into_iter().filter(|a| a.desktop && a.category == key).collect()),
    };
    if !key.starts_with("catalog:") {
        apps.sort_by_cached_key(|a| (a.source != nyx_store::nix::NYXOS && !a.popular, a.name.to_lowercase()));
    }
    let note = match apps.len() {
        1 => "1 app".to_string(),
        n => format!("{} apps", thousands(n)),
    };
    let note = if title != key && !key.contains(':') && key != "essentials" { format!("{key} · {note}") } else { note };
    heading(&title, &note);
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
    let here = if nyx_store::nix::active() {
        ""
    } else {
        " Here they install into your home folder from their published releases."
    };
    heading(
        "Nyx Apps",
        &format!("Apps made for NyxOS, in one design and working together.{here} {have} of {} installed.", apps.len()),
    );
    if apps.is_empty() {
        text("The Nyx apps' installer couldn't be reached. Refresh with Ctrl+R to try again.");
    }
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
    let installed = installed_list(c);
    if nyx_store::nix::active() {
        heading("Your apps", "Everything installed on this computer, from the NyxOS catalog, nixpkgs and Flathub.");
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
    if installed.is_empty() {
        text("No desktop apps installed from Store's sources yet.");
    } else {
        let apps: Vec<App> = installed.into_iter().cloned().collect();
        grid(&apps);
    }
}

/// What Installed lists: on NyxOS everything installed, once; on Arch the desktop
/// apps, each from the source it was installed from.
fn installed_list(c: &Catalog) -> Vec<&App> {
    if nyx_store::nix::active() {
        let installed: Vec<&App> = c.apps.iter().filter(|a| a.installed.is_some()).collect();
        let offered: BTreeSet<(String, String)> = installed.iter().flat_map(|a| a.also.iter().cloned()).collect();
        let mut seen = BTreeSet::new();
        return installed
            .into_iter()
            .filter(|a| !offered.contains(&(a.source.clone(), a.package.clone())))
            .filter(|a| seen.insert((a.source.as_str(), a.package.as_str())))
            .collect();
    }
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
        if seen.insert(a.package.as_str()) {
            apps.push(a);
        }
    }
    apps
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
        if c.apps.iter().any(|a| a.nyx) {
            sources.push(("nyx".into(), "Nyx Apps".into()));
        }
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

/// Apps as cards in up to three columns: the icon, the name and a line or two about
/// it, and a labelled Install or Open at the end. A card opens the app's page.
fn grid(apps: &[App]) {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .min_children_per_line(grid_columns())
        .max_children_per_line(3)
        .column_spacing(10)
        .row_spacing(10)
        .homogeneous(true)
        .build();
    grid.add_css_class("store-grid");
    let page = ui().page.borrow().clone();
    let searching = page == "search";
    // Where sources mix, each card says where it's from.
    let tagged = nyx_store::nix::active() && matches!(page.as_str(), "search" | "categories" | "installed");
    let shown: Vec<App> = apps.iter().take(200).cloned().collect();
    for app in &shown {
        let card = widgets::hbox(12);
        card.add_css_class("app-card");
        // Narrow enough for two columns in a tiled half-screen window.
        card.set_size_request(240, -1);
        if !app.summary.is_empty() {
            card.set_tooltip_text(Some(&app.summary));
        }
        let icon = app_icon(app, 44);
        icon.set_valign(gtk::Align::Center);
        card.append(&icon);
        let text = widgets::vbox(1);
        text.set_hexpand(true);
        text.set_valign(gtk::Align::Center);
        // Capped natural widths, so long summaries don't cost the grid its columns.
        let name = widgets::label(&app.name, "app-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        name.set_max_width_chars(22);
        text.append(&name);
        let about = widgets::label(if app.summary.is_empty() { &app.category } else { &app.summary }, "app-line");
        about.set_wrap(true);
        about.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        about.set_lines(2);
        about.set_ellipsize(gtk::pango::EllipsizeMode::End);
        about.set_max_width_chars(26);
        text.append(&about);
        let tags = widgets::hbox(6);
        if let Some(tag) = source_tag(app).filter(|t| tagged || *t == "AUR") {
            tags.append(&widgets::label(tag, "row-tag"));
        }
        if app.built_in {
            tags.append(&widgets::label("Built in", "row-tag"));
        } else if searching && app.installed.is_some() {
            tags.append(&widgets::label("Installed", "row-tag"));
        }
        if tags.first_child().is_some() {
            tags.set_margin_top(3);
            text.append(&tags);
        }
        card.append(&text);
        card.append(&pill(app));
        grid.insert(&card, -1);
    }
    grid.connect_child_activated(move |_, child| {
        if let Some(app) = shown.get(child.index() as usize) {
            details(app.clone());
        }
    });
    ui().body.append(&grid);
}

/// How many columns of app cards fit the page: three in a full window, two in a tiled
/// half-screen one. Set from the width, since the grid would go by its cards' natural
/// widths and give up columns to long summaries.
fn grid_columns() -> u32 {
    let u = ui();
    let width = if u.window.width() > 0 { u.window.width() } else { u.window.default_width() };
    let sidebar = if u.nav.has_css_class("compact") { 56 } else { 212 };
    let content = (width - sidebar - 72).min(1000);
    ((content + 10) / (290 + 10)).clamp(1, 3) as u32
}

/// What a card does directly, as a labelled pill: Install an app that isn't installed,
/// Open one that is, or a note for an installed app without a launcher.
fn pill(app: &App) -> gtk::Widget {
    if app.installed.is_some() && app.desktop_id.is_empty() {
        let done = widgets::label(if app.built_in { "Built in" } else { "Installed" }, "pill-note");
        done.set_valign(gtk::Align::Center);
        return done.upcast();
    }
    let (label, tip) = if app.installed.is_some() {
        ("Open", format!("Open {}", app.name))
    } else if app.source == "aur" {
        ("Install", format!("Review the build of {}", app.name))
    } else {
        ("Install", format!("Install {}", app.name))
    };
    let b = gtk::Button::with_label(label);
    b.add_css_class("pill");
    b.set_tooltip_text(Some(&tip));
    b.update_property(&[gtk::accessible::Property::Label(&tip)]);
    b.set_valign(gtk::Align::Center);
    let a = app.clone();
    if app.installed.is_some() {
        b.add_css_class("open");
        b.connect_clicked(move |_| launch(&a));
    } else {
        b.set_sensitive(!ui().busy.get());
        b.connect_clicked(move |_| install(&a));
    }
    b.upcast()
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
/// app after the review of its build files, a Nyx app on Arch after the review of what
/// the Nyx installer will do.
fn install(app: &App) {
    if app.source == "aur" {
        review_aur(&app.package);
    } else if let Some(name) = app.suite_name() {
        start_request(Request::NyxApps { change: "install".into(), apps: vec![name.into()] });
    } else {
        start_request(Request::Install { source: app.source.clone(), packages: vec![app.package.clone()] });
    }
}

/// A source as people read it.
fn source_name(source: &str) -> &str {
    match source {
        "nyxos" => "NyxOS catalog",
        "nixpkgs" => "nixpkgs",
        "flathub" => "Flathub",
        "aur" => "AUR",
        nyx_store::suite::SUITE => "Nyx installer",
        other => other,
    }
}

/// Where an app comes from, as its page and the spotlight say it: "Arch Linux · extra".
fn source_label(app: &App) -> String {
    match app.source.as_str() {
        s @ ("core" | "extra" | "multilib") => format!("Arch Linux · {s}"),
        "omarchy" => "Omarchy".into(),
        "aur" => "AUR · community recipe".into(),
        "blackarch" => "BlackArch".into(),
        nyx_store::suite::SUITE => "Nyx · published release".into(),
        "unknown" | "local" => "Installed outside Store".into(),
        s => source_name(s).into(),
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
    // The Nyx apps have no versions to compare here: updating installs each one's latest
    // release, as `nyx update` does.
    let nyx: Vec<String> = c
        .apps
        .iter()
        .filter(|a| a.installed.is_some())
        .filter_map(|a| a.suite_name())
        .filter(|name| c.suite.install.contains(*name))
        .map(String::from)
        .collect();
    heading("Nyx app updates", "The Nyx installer puts each Nyx app's latest release in place of the one you have.");
    if nyx.is_empty() {
        text("No Nyx apps installed from their releases.");
        return;
    }
    let count = nyx.len();
    let (row, update) = widgets::button_row(
        "Update the Nyx apps",
        &format!("{count} Nyx {} installed", if count == 1 { "app" } else { "apps" }),
        "Review",
        move |_| start_request(Request::NyxApps { change: "update".into(), apps: nyx.clone() }),
    );
    update.set_sensitive(!u.busy.get());
    u.body.append(&row);
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

/// How long ago a time was, short enough for the sidebar: "5 min ago".
fn ago_short(time: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    match (now - time).max(0) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86400 => format!("{} h ago", s / 3600),
        s if s < 2 * 86400 => "yesterday".into(),
        s => format!("{} days ago", s / 86400),
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
    let add = button("Add a source…", || repository_dialog(None));
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
    u.status.set_text(if matches!(request, Request::NyxApps { .. }) {
        "Reviewing Nyx apps…"
    } else {
        "Authorizing package operation…"
    });
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
    let suite = !plan.changes.is_empty() && plan.changes.iter().all(|c| c.source == nyx_store::suite::SUITE);
    let note = widgets::label(
        &if suite {
            let one = plan.changes.len() == 1;
            let count = if one { "1 Nyx app".to_string() } else { format!("{} Nyx apps", plan.changes.len()) };
            if plan.changes.iter().all(|c| c.new.is_none()) {
                format!(
                    "{count}. The Nyx uninstaller removes {} from your home folder and keeps {} settings and data, the same as running the command below.",
                    if one { "it" } else { "them" },
                    if one { "its" } else { "their" },
                )
            } else {
                format!(
                    "{count}. {} into your home folder (~/.local) with the Nyx installer, the same as running the command below. No password is needed, but an app that needs Arch packages you don't have yet stops with an error: install those first.",
                    if one { "Its latest published release installs" } else { "Their latest published releases install" },
                )
            }
        } else if nyx_store::nix::active() && plan.changes.iter().all(|c| c.source == "flathub") {
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
    if suite {
        let action = if plan.changes.iter().all(|c| c.new.is_none()) { "remove" } else { "install" };
        let apps: Vec<String> = plan
            .changes
            .iter()
            .filter_map(|c| {
                let u = ui();
                let catalog = u.catalog.borrow();
                catalog.as_ref()?.apps.iter().find(|a| a.name == c.name)?.suite_name().map(String::from)
            })
            .collect();
        let command = widgets::label(&nyx_store::suite::command(action, &apps), "dim");
        command.add_css_class("monospace");
        command.set_selectable(true);
        command.set_focusable(false);
        command.set_wrap(true);
        command.set_wrap_mode(gtk::pango::WrapMode::Char);
        let attrs = gtk::pango::AttrList::new();
        attrs.insert(gtk::pango::AttrInt::new_insert_hyphens(false));
        command.set_attributes(Some(&attrs));
        command.set_xalign(0.0);
        card.append(&command);
    }
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
