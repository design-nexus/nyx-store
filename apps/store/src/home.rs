//! Home: the spotlight, the collections and shelves of apps, each section in its own
//! colour from the theme's swatches.
use super::*;

pub(super) fn render(catalog: &Catalog) {
    let mut seen = BTreeSet::new();
    let apps: Vec<App> = catalog
        .apps
        .iter()
        .filter(|a| a.desktop && repositories::official(&a.source) && source_match(a))
        .filter(|a| seen.insert(a.package.clone()))
        .cloned()
        .collect();
    if apps.is_empty() {
        text("Select All sources to discover desktop apps.");
        return;
    }
    featured(&apps);
    collections(&apps);
    let picks: Vec<App> = ["firefox", "libreoffice-fresh", "keepassxc", "vlc", "signal-desktop", "obsidian"]
        .iter()
        .filter_map(|name| apps.iter().find(|a| a.package == *name).cloned())
        .collect();
    let from = if nyx_store::nix::active() { "A few great places to start." } else { "A few great places to start, handpicked from Arch." };
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
        shelf(title, copy, category_icon(category), theme::tone(category), &selection, Some(category));
    }
}

fn category_icon(category: &str) -> &'static str {
    catalog::CATEGORIES.iter().find(|(c, _)| *c == category).map(|(_, i)| *i).unwrap_or("view-app-grid-symbolic")
}

/// A section of apps: its chip, title and note, See all for a category, then the apps.
fn shelf(title: &str, note: &str, icon: &str, tone: &str, apps: &[App], see_all: Option<&'static str>) {
    if apps.is_empty() {
        return;
    }
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
    if let Some(category) = see_all {
        let more = widgets::bar_button("go-next-symbolic", &format!("See all {}", category.to_lowercase()));
        more.connect_clicked(move |_| category_page(category));
        more.set_valign(gtk::Align::Center);
        header.append(&more);
    }
    ui().body.append(&header);
    grid(apps);
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
            &format!("{}{}", app.source, match (app.installed.is_some(), nyx_store::nix::active()) {
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
