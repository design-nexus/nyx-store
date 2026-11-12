//! Home: the spotlight, the collections and shelves of apps, each section in its own
//! colour from the theme's swatches. On NyxOS, the Nyx apps and the catalog's bundles too.
use super::*;
use nyx_store::nix::NYXOS;

pub(super) fn render(catalog: &Catalog) {
    let nix = nyx_store::nix::active();
    let apps: Vec<App> = browsable(catalog)
        .into_iter()
        .filter(|a| a.desktop && !a.built_in && (repositories::official(&a.source) || a.source == NYXOS))
        .cloned()
        .collect();
    if apps.is_empty() {
        text("The catalog has no apps yet. Refresh it with Ctrl+R.");
        return;
    }
    featured(&apps);
    collections(&apps);
    if nix {
        nyx_shelf(catalog);
        bundles(catalog);
    }
    let picks: Vec<App> =
        ["firefox", "libreoffice-fresh", "libreoffice", "keepassxc", "vlc", "signal-desktop", "signal", "obsidian"]
            .iter()
            .filter_map(|name| apps.iter().find(|a| a.package == *name).cloned())
            .take(6)
            .collect();
    let from = if nix { "A few great places to start." } else { "A few great places to start, handpicked from Arch." };
    shelf("Everyday essentials", from, "starred-symbolic", "accent", &picks, None);
    for (category, title, copy) in [
        ("Graphics", "Make something extraordinary", "From the first sketch to the final frame."),
        ("Audio & video", "Your own creative studio", "Record it. Cut it. Share it with the world."),
        ("Games", "Clock out. Play on.", "A little adventure belongs on every desktop."),
        ("Development", "Build what comes next", "Great tools for your next big idea."),
    ] {
        let mut selection: Vec<App> = apps.iter().filter(|a| a.category == category).cloned().collect();
        selection.sort_by_key(|a| !a.popular);
        selection.truncate(6);
        let see_all = SeeAll(format!("See all {}", category.to_lowercase()), Box::new(move || category_page(category)));
        shelf(title, copy, category_icon(category), theme::tone(category), &selection, Some(see_all));
    }
}

/// Made for NyxOS: the Nyx apps not installed yet first, and See all for the Nyx Apps page.
fn nyx_shelf(catalog: &Catalog) {
    let mut apps: Vec<App> = catalog.apps.iter().filter(|a| a.nyx && !a.built_in).cloned().collect();
    apps.sort_by_key(|a| (a.installed.is_some(), !a.popular));
    apps.truncate(6);
    let see_all = SeeAll("See all Nyx apps".into(), Box::new(|| navigate("nyx")));
    shelf("Made for NyxOS", "The Nyx suite: one design, working together.", "view-app-grid-symbolic", "accent", &apps, Some(see_all));
}

/// The catalog's bundles: apps for one kind of work, reviewed and installed together.
fn bundles(catalog: &Catalog) {
    if catalog.bundles.is_empty() {
        return;
    }
    shelf_head("Bundles", "Everything for one kind of work, installed in one go.", "folder-download-symbolic", "accent", None);
    let grid = tile_grid();
    for bundle in &catalog.bundles {
        let missing = catalog
            .apps
            .iter()
            .filter(|a| a.source == NYXOS && a.installed.is_none() && bundle.items.contains(&a.package))
            .count();
        let line = match missing {
            0 => format!("{} apps · all installed", bundle.items.len()),
            n => format!("{} apps · {n} to install", bundle.items.len()),
        };
        let tone = match bundle.id.as_str() {
            "everyday" => "amber",
            "developer" => "teal",
            "ai-developer" => "violet",
            "homelab" => "blue",
            _ => "graphite",
        };
        let b = bundle.clone();
        let t = tile(&bundle.name, &line, "folder-download-symbolic", tone, move || bundle_dialog(&b));
        t.set_tooltip_text(Some(&bundle.summary));
        grid.insert(&t, -1);
    }
    ui().body.append(&grid);
}

/// A bundle's apps, those installed marked, and Install for the rest: one review.
fn bundle_dialog(bundle: &catalog::Bundle) {
    let u = ui();
    let catalog = u.catalog.borrow();
    let Some(c) = catalog.as_ref() else { return };
    let (dialog, card) = widgets::dialog(&bundle.name, 560);
    dialog.set_default_height(560);
    let note = widgets::label(&bundle.summary, "dim");
    note.set_wrap(true);
    note.set_xalign(0.0);
    card.append(&note);
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    let mut missing = Vec::new();
    for id in &bundle.items {
        let Some(app) = c.apps.iter().find(|a| a.source == NYXOS && a.package == *id) else { continue };
        let state = widgets::label("Installed", "source-tag");
        list.append(&widgets::row(&app.name, &app.summary, app.installed.is_some().then(|| state.upcast_ref())));
        if app.installed.is_none() {
            missing.push(id.clone());
        }
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(200)
        .max_content_height(420)
        .propagate_natural_height(true)
        .build();
    card.append(&scroll);
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    let close = button("Not now", move || d.close());
    actions.append(&close);
    let label = match missing.len() {
        0 => "All installed".to_string(),
        1 => "Install 1 app".to_string(),
        n => format!("Install {n} apps"),
    };
    let d = dialog.clone();
    let install = button(&label, move || {
        d.close();
        start_request(Request::Install { source: NYXOS.into(), packages: missing.clone() });
    });
    install.add_css_class("suggested-action");
    install.set_sensitive(label != "All installed" && !u.busy.get());
    actions.append(&install);
    card.append(&actions);
    dialog.present();
    close.grab_focus();
}

fn category_icon(category: &str) -> &'static str {
    catalog::CATEGORIES.iter().find(|(c, _)| *c == category).map(|(_, i)| *i).unwrap_or("view-app-grid-symbolic")
}

/// A shelf's See all: its tooltip and where it goes.
struct SeeAll(String, Box<dyn Fn()>);

/// A section of apps: its heading, then the apps.
fn shelf(title: &str, note: &str, icon: &str, tone: &str, apps: &[App], see_all: Option<SeeAll>) {
    if apps.is_empty() {
        return;
    }
    shelf_head(title, note, icon, tone, see_all);
    grid(apps);
}

/// A shelf's heading: its chip, title and note, and See all.
fn shelf_head(title: &str, note: &str, icon: &str, tone: &str, see_all: Option<SeeAll>) {
    let header = widgets::hbox(12);
    header.add_css_class("shelf-head");
    header.append(&tone_chip(icon, tone, 30));
    let words = widgets::vbox(1);
    words.set_hexpand(true);
    words.set_valign(gtk::Align::Center);
    let t = widgets::label(title, "shelf-title");
    t.set_xalign(0.0);
    t.set_wrap(true);
    words.append(&t);
    let n = widgets::label(note, "shelf-note");
    n.set_xalign(0.0);
    n.set_wrap(true);
    words.append(&n);
    header.append(&words);
    if let Some(SeeAll(tip, go)) = see_all {
        let more = widgets::bar_button("go-next-symbolic", &tip);
        more.connect_clicked(move |_| go());
        more.set_valign(gtk::Align::Center);
        header.append(&more);
    }
    ui().body.append(&header);
}

fn category_page(category: &str) {
    let u = ui();
    *u.page.borrow_mut() = "categories".into();
    *u.category.borrow_mut() = category.into();
    prefs::update(|p| p.last_section = "categories".into());
    u.scroll.vadjustment().set_value(0.0);
    super::render();
}

fn featured(apps: &[App]) {
    let scenes: Vec<(&App, &str, &str, &str)> = [
        ("blender", "MAKE SOMETHING BIG", "Bring your imagination into another dimension.", "coral"),
        ("krita", "A CANVAS WITHOUT LIMITS", "A whole studio of brushes. One blank canvas. Your next idea.", "pink"),
        ("obs-studio", "READY FOR YOUR CLOSE-UP", "Turn your screen into a studio, and your ideas into a broadcast.", "blue"),
    ]
    .into_iter()
    .filter_map(|(package, title, copy, tone)| apps.iter().find(|a| a.package == package).map(|a| (a, title, copy, tone)))
    .collect();
    if scenes.is_empty() {
        return;
    }
    let stack = gtk::Stack::new();
    stack.set_hexpand(true);
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(if prefs::get().reduce_motion { 0 } else { 160 });
    for (index, (app, title, copy, tone)) in scenes.iter().enumerate() {
        let tile = gtk::Button::new();
        tile.add_css_class("feature-hero");
        tile.add_css_class(&format!("tone-{tone}"));
        tile.set_overflow(gtk::Overflow::Hidden);
        tile.set_tooltip_text(Some(&format!("Explore {}", app.name)));
        let overlay = gtk::Overlay::new();
        overlay.set_overflow(gtk::Overflow::Hidden);
        let row = widgets::hbox(12);
        row.add_css_class("feature-content");
        let words = widgets::vbox(8);
        words.set_hexpand(true);
        words.set_valign(gtk::Align::Center);
        let eyebrow = widgets::label(title, "feature-eyebrow");
        eyebrow.set_xalign(0.0);
        eyebrow.set_wrap(true);
        words.append(&eyebrow);
        let name = widgets::label(&app.name, "feature-title");
        name.set_xalign(0.0);
        name.set_wrap(true);
        words.append(&name);
        let description = widgets::label(copy, "feature-description");
        description.set_xalign(0.0);
        description.set_wrap(true);
        description.set_max_width_chars(36);
        words.append(&description);
        let action = widgets::label(&format!("Explore {}  →", app.name), "feature-action");
        action.set_halign(gtk::Align::Start);
        words.append(&action);
        let tag = widgets::label(
            &format!("{}{}", source_name(&app.source), match (app.installed.is_some(), nyx_store::nix::active()) {
                (true, _) => " · In your library",
                (false, true) => " · Available",
                (false, false) => " · Available on Arch",
            }),
            "source-tag",
        );
        tag.set_xalign(0.0);
        words.append(&tag);
        row.append(&words);
        let art = app_icon(app, 96);
        art.add_css_class("feature-app-icon");
        art.set_valign(gtk::Align::Center);
        row.append(&art);
        let backdrop = gtk::Picture::new();
        backdrop.set_can_shrink(true);
        backdrop.set_content_fit(gtk::ContentFit::Cover);
        let base = widgets::hbox(0);
        base.add_css_class("feature-base");
        overlay.set_child(Some(&base));
        overlay.add_overlay(&backdrop);
        // Overlays never contribute the screenshot's natural width to the window.
        let shade = widgets::hbox(0);
        shade.add_css_class("feature-shade");
        overlay.add_overlay(&shade);
        overlay.add_overlay(&row);
        overlay.set_measure_overlay(&row, true);
        load_picture(app, 0, &backdrop);
        tile.set_child(Some(&overlay));
        let app = (*app).clone();
        tile.connect_clicked(move |_| details(app.clone()));
        stack.add_named(&tile, Some(&index.to_string()));
    }
    ui().body.append(&stack);

    // Under the spotlight: a dot per app, the current one lit, and the arrows.
    let controls = widgets::hbox(6);
    controls.add_css_class("feature-controls");
    let dots = widgets::hbox(6);
    dots.add_css_class("page-dots");
    dots.set_hexpand(true);
    dots.set_valign(gtk::Align::Center);
    let count = scenes.len();
    let marks: Vec<gtk::Box> = (0..count)
        .map(|i| {
            let dot = widgets::hbox(0);
            dot.add_css_class("page-dot");
            if i == 0 {
                dot.add_css_class("current");
            }
            dots.append(&dot);
            dot
        })
        .collect();
    controls.append(&dots);
    let current = Rc::new(Cell::new(0usize));
    let marks = Rc::new(marks);
    for (icon, tip, forwards) in
        [("go-previous-symbolic", "Previous featured app", false), ("go-next-symbolic", "Next featured app", true)]
    {
        let arrow = widgets::bar_button(icon, tip);
        let (stack, current, marks) = (stack.clone(), current.clone(), marks.clone());
        arrow.connect_clicked(move |_| {
            let index = if forwards { (current.get() + 1) % count } else { (current.get() + count - 1) % count };
            marks[current.get()].remove_css_class("current");
            marks[index].add_css_class("current");
            current.set(index);
            stack.set_visible_child_name(&index.to_string());
        });
        controls.append(&arrow);
    }
    ui().body.append(&controls);
}

/// Four ways in, each a short tile in its category's colour.
fn collections(apps: &[App]) {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(2)
        .max_children_per_line(4)
        .column_spacing(12)
        .row_spacing(12)
        .homogeneous(true)
        .build();
    grid.add_css_class("home-collections");
    for (title, category, icon) in [
        ("Create", "Graphics", "applications-graphics-symbolic"),
        ("Work", "Productivity", "accessories-text-editor-symbolic"),
        ("Play", "Games", "applications-games-symbolic"),
        ("Develop", "Development", "utilities-terminal-symbolic"),
    ] {
        let tone = theme::tone(category);
        let tile = button(title, move || category_page(category));
        tile.add_css_class("collection-tile");
        tile.add_css_class(&format!("tone-{tone}"));
        tile.set_tooltip_text(Some(&format!("Browse {}", category.to_lowercase())));
        let content = widgets::hbox(12);
        content.append(&tone_chip(icon, tone, 36));
        let words = widgets::vbox(1);
        words.set_valign(gtk::Align::Center);
        let name = widgets::label(title, "collection-name");
        name.set_xalign(0.0);
        words.append(&name);
        let count = apps.iter().filter(|a| a.category == category).count();
        let n = widgets::label(&format!("{count} apps"), "collection-count");
        n.set_xalign(0.0);
        words.append(&n);
        content.append(&words);
        tile.set_child(Some(&content));
        grid.insert(&tile, -1);
    }
    ui().body.append(&grid);
}
