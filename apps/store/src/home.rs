//! Home: a short spotlight with its Install button, then sections of apps as cards
//! with labelled Install and Open, the Nyx apps and the NyxOS catalog's bundles.
use super::*;
use nyx_store::nix::NYXOS;

pub(super) fn render(catalog: &Catalog) {
    let tier = ui().tier.get();
    let narrow = tier == 0;
    let apps: Vec<App> = browsable(catalog)
        .into_iter()
        .filter(|a| a.desktop && !a.built_in && (repositories::official(&a.source) || a.source == NYXOS))
        .cloned()
        .collect();
    if apps.is_empty() {
        text("The catalog has no apps yet. Refresh it with Ctrl+R.");
        return;
    }
    let shelf_size = if narrow { 4 } else { 6 };
    featured(&apps, tier == 2);
    let picks: Vec<App> =
        ["firefox", "libreoffice-fresh", "libreoffice", "keepassxc", "vlc", "signal-desktop", "signal", "obsidian"]
            .iter()
            .filter_map(|name| apps.iter().find(|a| a.package == *name).cloned())
            .take(shelf_size)
            .collect();
    if !picks.is_empty() {
        let more = More::Link("See all".into(), "See every everyday essential".into(), Box::new(|| category_page("essentials")));
        section_head("Everyday essentials", "starred-symbolic", more);
        grid(&picks);
    }
    nyx_shelf(catalog, narrow);
    bundles(catalog, narrow);
    for (category, title) in [
        ("Graphics", "Make something extraordinary"),
        ("Audio & video", "Your own creative studio"),
        ("Games", "Clock out. Play on."),
        ("Development", "Build what comes next"),
    ] {
        let mut selection: Vec<App> = apps.iter().filter(|a| a.category == category).cloned().collect();
        selection.sort_by_key(|a| !a.popular);
        selection.truncate(shelf_size);
        if selection.is_empty() {
            continue;
        }
        let tip = format!("See all {} apps", category.to_lowercase());
        section_head(title, category_icon(category), More::Link("See all".into(), tip, Box::new(move || category_page(category))));
        grid(&selection);
    }
}

/// What a section heading has at its right: a link (its label, tooltip and where it
/// goes) or a note.
enum More {
    Link(String, String, Box<dyn Fn()>),
    Note(&'static str),
}

/// A section's heading: its icon in the accent, its title, and See all or a note.
fn section_head(title: &str, icon: &str, more: More) {
    let head = widgets::hbox(8);
    head.add_css_class("section-head");
    let image = gtk::Image::from_icon_name(icon);
    image.add_css_class("section-icon");
    head.append(&image);
    let t = widgets::label(title, "section-name");
    t.set_hexpand(true);
    t.set_ellipsize(gtk::pango::EllipsizeMode::End);
    head.append(&t);
    match more {
        More::Link(label, tip, go) => {
            let b = gtk::Button::with_label(&label);
            b.add_css_class("see-all");
            b.set_tooltip_text(Some(&tip));
            b.update_property(&[gtk::accessible::Property::Label(&tip)]);
            b.connect_clicked(move |_| go());
            head.append(&b);
        }
        More::Note(note) => head.append(&widgets::label(note, "section-note")),
    }
    ui().body.append(&head);
}

/// Made for NyxOS: the Nyx apps not installed yet first, as a row of tall cards, and
/// See all for the Nyx apps page.
fn nyx_shelf(catalog: &Catalog, narrow: bool) {
    let mut apps: Vec<App> = catalog.apps.iter().filter(|a| a.nyx && !a.built_in).cloned().collect();
    if apps.is_empty() {
        return;
    }
    let total = catalog.apps.iter().filter(|a| a.nyx).count();
    apps.sort_by_key(|a| (a.installed.is_some(), !a.popular));
    let count = if narrow { 4 } else { 5 };
    apps.truncate(count);
    let more = More::Link(format!("See all {total}"), "See all the Nyx apps".into(), Box::new(|| navigate("nyx")));
    section_head("Made for NyxOS", "view-app-grid-symbolic", more);
    let row = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .min_children_per_line(count as u32)
        .max_children_per_line(count as u32)
        .column_spacing(10)
        .row_spacing(10)
        .homogeneous(true)
        .build();
    row.add_css_class("store-grid");
    row.add_css_class("nyx-row");
    for app in &apps {
        let card = widgets::vbox(8);
        card.add_css_class("nyx-card");
        let icon = app_icon(app, 52);
        icon.set_halign(gtk::Align::Center);
        card.append(&icon);
        let name = widgets::label(app.name.strip_prefix("Nyx ").unwrap_or(&app.name), "app-name");
        name.set_halign(gtk::Align::Center);
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        card.append(&name);
        let action = pill(app);
        action.set_halign(gtk::Align::Center);
        card.append(&action);
        if !app.summary.is_empty() {
            card.set_tooltip_text(Some(&app.summary));
        }
        row.insert(&card, -1);
    }
    row.connect_child_activated(move |_, child| {
        if let Some(app) = apps.get(child.index() as usize) {
            details(app.clone());
        }
    });
    ui().body.append(&row);
}

/// One of a bundle's items as its card and review show it: its name, what it installs,
/// an app for its icon, whether it's all installed, and on Arch its AUR package to build.
struct Entry {
    name: String,
    detail: String,
    icon: App,
    installed: bool,
    aur: Option<String>,
}

/// A bundle's items: on NyxOS the catalog's apps; on Arch what each installs here, from
/// the repositories, the AUR or the Nyx installer.
fn entries(c: &Catalog, bundle: &catalog::Bundle) -> Vec<Entry> {
    if nyx_store::nix::active() {
        return bundle
            .items
            .iter()
            .filter_map(|id| c.apps.iter().find(|a| a.source == NYXOS && a.package == *id))
            .map(|a| Entry {
                name: a.name.clone(),
                detail: a.summary.clone(),
                icon: a.clone(),
                installed: a.installed.is_some(),
                aur: None,
            })
            .collect();
    }
    bundle
        .arch
        .iter()
        .map(|item| {
            let have = |(source, package): &(String, String)| {
                if source == nyx_store::suite::SUITE {
                    c.apps.iter().any(|a| &a.source == source && &a.package == package && a.installed.is_some())
                } else {
                    c.installed.contains_key(package)
                }
            };
            let first = &item.packages[0];
            let icon = c
                .apps
                .iter()
                .find(|a| a.source == first.0 && a.package == first.1)
                .cloned()
                .unwrap_or_else(|| App { package: first.1.clone(), name: item.name.clone(), ..Default::default() });
            let mut detail = item
                .packages
                .iter()
                .map(|(source, package)| {
                    if source == nyx_store::suite::SUITE {
                        "Nyx app".to_string()
                    } else {
                        format!("{package} · {}", source_name(source))
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            if !item.missing.is_empty() {
                detail.push_str(&format!(" · without {}", item.missing.join(", ")));
            }
            Entry {
                name: item.name.clone(),
                detail,
                icon,
                installed: item.packages.iter().all(have),
                aur: item.packages.iter().find(|p| p.0 == "aur" && !have(p)).map(|p| p.1.clone()),
            }
        })
        .collect()
}

/// Names as a sentence: "A, B and C", or the first few and how many more.
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [first @ .., last] if first.len() < 3 => format!("{} and {last}", first.join(", ")),
        _ => format!("{} and {} more", names[..2].join(", "), names.len() - 2),
    }
}

/// The catalog's bundles: apps for one kind of work, reviewed and installed together.
/// On Arch a card says which of its items Arch doesn't have.
fn bundles(catalog: &Catalog, narrow: bool) {
    let shown: Vec<catalog::Bundle> =
        catalog.bundles.iter().filter(|b| !entries(catalog, b).is_empty()).cloned().collect();
    if shown.is_empty() {
        return;
    }
    section_head("Bundles", "folder-download-symbolic", More::Note("Install a whole set in one go"));
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .activate_on_single_click(true)
        .min_children_per_line(if narrow { 1 } else { 2 })
        .max_children_per_line(if narrow { 1 } else { 2 })
        .column_spacing(10)
        .row_spacing(10)
        .homogeneous(true)
        .build();
    grid.add_css_class("store-grid");
    for bundle in &shown {
        let items = entries(catalog, bundle);
        let missing = items.iter().filter(|e| !e.installed).count();
        let card = widgets::hbox(14);
        card.add_css_class("bundle-card");
        // The first three apps' icons, overlapping; desktop apps first, which have icons.
        let icons = gtk::Fixed::new();
        icons.add_css_class("bundle-icons");
        icons.set_valign(gtk::Align::Center);
        let firsts = items.iter().filter(|e| e.icon.desktop).chain(items.iter().filter(|e| !e.icon.desktop));
        for (i, entry) in firsts.take(3).enumerate() {
            icons.put(&app_icon(&entry.icon, 30), i as f64 * 22.0, 0.0);
        }
        card.append(&icons);
        let words = widgets::vbox(1);
        words.set_hexpand(true);
        words.set_valign(gtk::Align::Center);
        let name = widgets::label(&bundle.name, "app-name");
        name.set_ellipsize(gtk::pango::EllipsizeMode::End);
        words.append(&name);
        let line = widgets::label(&bundle.summary, "app-line");
        line.set_ellipsize(gtk::pango::EllipsizeMode::End);
        words.append(&line);
        if !bundle.left_out.is_empty() {
            let out = widgets::label(&format!("Not on Arch: {}", listed(&bundle.left_out)), "bundle-left-out");
            out.set_ellipsize(gtk::pango::EllipsizeMode::End);
            out.set_tooltip_text(Some(&format!("Arch and the AUR have nothing for {}", bundle.left_out.join(", "))));
            words.append(&out);
        }
        card.append(&words);
        if missing == 0 {
            let done = widgets::label("Installed", "pill-note");
            done.set_valign(gtk::Align::Center);
            card.append(&done);
        } else {
            let get = gtk::Button::with_label(&format!("Install {missing}"));
            get.add_css_class("pill");
            get.set_valign(gtk::Align::Center);
            get.set_tooltip_text(Some(&format!("Review and install {}", bundle.name)));
            get.set_sensitive(!ui().busy.get());
            let b = bundle.clone();
            get.connect_clicked(move |_| bundle_dialog(&b));
            card.append(&get);
        }
        card.set_tooltip_text(Some(&bundle.summary));
        grid.insert(&card, -1);
    }
    grid.connect_child_activated(move |_, child| {
        if let Some(bundle) = shown.get(child.index() as usize) {
            bundle_dialog(bundle);
        }
    });
    ui().body.append(&grid);
}

/// What installing a bundle on Arch takes: each repository's packages, then the Nyx
/// apps, each reviewed in turn. AUR packages build one at a time from their own review.
fn arch_requests(c: &Catalog, bundle: &catalog::Bundle) -> Vec<Request> {
    let mut repos: Vec<(String, Vec<String>)> = Vec::new();
    let mut nyx = Vec::new();
    for (source, package) in bundle.arch.iter().flat_map(|i| &i.packages) {
        if source == nyx_store::suite::SUITE {
            let app = c.apps.iter().find(|a| &a.source == source && &a.package == package);
            if let Some(name) = app.filter(|a| a.installed.is_none()).and_then(|a| a.suite_name())
                && !nyx.iter().any(|n| n == name)
            {
                nyx.push(name.to_string());
            }
        } else if source != "aur" && !c.installed.contains_key(package) {
            match repos.iter_mut().find(|(s, _)| s == source) {
                Some((_, packages)) if !packages.contains(package) => packages.push(package.clone()),
                Some(_) => {}
                None => repos.push((source.clone(), vec![package.clone()])),
            }
        }
    }
    let mut requests: Vec<Request> =
        repos.into_iter().map(|(source, packages)| Request::Install { source, packages }).collect();
    if !nyx.is_empty() {
        requests.push(Request::NyxApps { change: "install".into(), apps: nyx });
    }
    requests
}


/// A bundle's apps, those installed marked, and Install for the rest: one review (on
/// Arch one per repository, and one for the Nyx apps).
fn bundle_dialog(bundle: &catalog::Bundle) {
    let u = ui();
    let catalog = u.catalog.borrow();
    let Some(c) = catalog.as_ref() else { return };
    let nix = nyx_store::nix::active();
    let (dialog, card) = widgets::dialog(&bundle.name, 560);
    dialog.set_default_height(560);
    let note = widgets::label(&bundle.summary, "dim");
    note.set_wrap(true);
    note.set_xalign(0.0);
    card.append(&note);
    let list = widgets::vbox(0);
    list.add_css_class("group-list");
    for entry in entries(c, bundle) {
        let control: Option<gtk::Widget> = if entry.installed {
            Some(widgets::label("Installed", "source-tag").upcast())
        } else if let Some(package) = entry.aur.clone() {
            let d = dialog.clone();
            let review = button("Review build", move || {
                d.close();
                review_aur(&package);
            });
            review.set_tooltip_text(Some("AUR packages build one at a time, after you review their recipe"));
            review.set_sensitive(!u.busy.get());
            Some(review.upcast())
        } else {
            None
        };
        list.append(&widgets::row(&entry.name, &entry.detail, control.as_ref()));
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
    if !nix && !bundle.left_out.is_empty() {
        let out = widgets::label(
            &format!("Left out, with no package of the same name on Arch or the AUR: {}.", bundle.left_out.join(", ")),
            "dim",
        );
        out.set_wrap(true);
        out.set_xalign(0.0);
        card.append(&out);
    }
    let services: Vec<String> = bundle.arch.iter().flat_map(|i| i.services.iter().cloned()).collect::<BTreeSet<_>>().into_iter().collect();
    if !nix && !services.is_empty() {
        let text = format!(
            "On NyxOS these turn on services ({}). Here Store installs their packages and leaves the services off; turn them on with systemctl when you want them.",
            services.join(", ")
        );
        card.append(&widgets::banner(&glib::markup_escape_text(&text), false));
    }
    let actions = widgets::hbox(8);
    actions.set_halign(gtk::Align::End);
    let d = dialog.clone();
    let close = button("Not now", move || d.close());
    actions.append(&close);
    let (label, requests) = if nix {
        let missing: Vec<String> = bundle
            .items
            .iter()
            .filter(|id| c.apps.iter().any(|a| a.source == NYXOS && a.package == **id && a.installed.is_none()))
            .cloned()
            .collect();
        let label = match missing.len() {
            0 => String::new(),
            1 => "Install 1 app".to_string(),
            n => format!("Install {n} apps"),
        };
        let requests =
            if missing.is_empty() { Vec::new() } else { vec![Request::Install { source: NYXOS.into(), packages: missing }] };
        (label, requests)
    } else {
        let requests = arch_requests(c, bundle);
        let count: usize = requests
            .iter()
            .map(|r| match r {
                Request::Install { packages, .. } => packages.len(),
                Request::NyxApps { apps, .. } => apps.len(),
                _ => 0,
            })
            .sum();
        let label = match count {
            0 => String::new(),
            1 => "Install 1 package".to_string(),
            n => format!("Install {n} packages"),
        };
        (label, requests)
    };
    let d = dialog.clone();
    let install = button(if label.is_empty() { "All installed" } else { &label }, move || {
        d.close();
        let mut queue: VecDeque<Request> = requests.clone().into();
        if let Some(first) = queue.pop_front() {
            *ui().queue.borrow_mut() = queue;
            start_request(first);
        }
    });
    install.add_css_class("suggested-action");
    install.set_sensitive(!label.is_empty() && !u.busy.get());
    actions.append(&install);
    card.append(&actions);
    dialog.present();
    close.grab_focus();
}

fn category_icon(category: &str) -> &'static str {
    catalog::CATEGORIES.iter().find(|(c, _)| *c == category).map(|(_, i)| *i).unwrap_or("view-app-grid-symbolic")
}

fn category_page(category: &str) {
    let u = ui();
    u.detail.borrow_mut().take();
    *u.page.borrow_mut() = "categories".into();
    *u.category.borrow_mut() = category.into();
    let section = if KINDS.iter().any(|k| k.1 == category) { format!("cat:{category}") } else { "categories".into() };
    prefs::update(|p| p.last_section = section);
    u.scroll.vadjustment().set_value(0.0);
    super::render();
}

/// The spotlight: a short banner in its app's colours with the words and Install at
/// the left, the app's screenshot at the right when there's room, and a dot per app.
fn featured(apps: &[App], wide: bool) {
    let scenes: Vec<(&App, &str, &str, &str)> = [
        ("blender", "MAKE SOMETHING BIG", "Model, animate and render in 3D, from the first sketch to the final frame.", "coral"),
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
        let hero = gtk::Overlay::new();
        hero.add_css_class("feature-hero");
        hero.add_css_class(&format!("tone-{tone}"));
        hero.set_overflow(gtk::Overflow::Hidden);
        hero.set_tooltip_text(Some(&format!("More about {}", app.name)));
        hero.set_cursor_from_name(Some("pointer"));
        let base = widgets::hbox(0);
        base.add_css_class("feature-base");
        hero.set_child(Some(&base));
        if wide {
            let shot = gtk::Picture::new();
            shot.add_css_class("feature-shot");
            shot.set_can_shrink(true);
            shot.set_content_fit(gtk::ContentFit::Cover);
            shot.set_overflow(gtk::Overflow::Hidden);
            // Its natural width is the screenshot's; the clamp keeps it a card.
            let frame = nyx_ui::clamp::Clamp::new(&shot, 360);
            frame.set_halign(gtk::Align::End);
            frame.set_valign(gtk::Align::End);
            frame.set_margin_top(26);
            frame.set_margin_end(26);
            hero.add_overlay(&frame);
            load_picture(app, 0, &shot);
        }
        let words = widgets::vbox(6);
        words.add_css_class("feature-words");
        words.set_valign(gtk::Align::Center);
        words.set_halign(gtk::Align::Start);
        let eyebrow = widgets::label(title, "feature-eyebrow");
        words.append(&eyebrow);
        let name = widgets::label(&app.name, "feature-title");
        words.append(&name);
        let description = widgets::label(copy, "feature-description");
        description.set_wrap(true);
        description.set_max_width_chars(if wide { 46 } else { 64 });
        words.append(&description);
        let actions = widgets::hbox(12);
        actions.set_margin_top(8);
        let action: gtk::Widget = if app.installed.is_some() && !app.desktop_id.is_empty() {
            let open = gtk::Button::with_label("Open");
            open.add_css_class("suggested-action");
            let a = (*app).clone();
            open.connect_clicked(move |_| launch(&a));
            open.upcast()
        } else if app.installed.is_some() {
            widgets::label("Installed", "feature-source").upcast()
        } else {
            let get = gtk::Button::new();
            get.add_css_class("suggested-action");
            let content = widgets::hbox(8);
            content.append(&gtk::Image::from_icon_name("folder-download-symbolic"));
            content.append(&gtk::Label::new(Some("Install")));
            get.set_child(Some(&content));
            get.set_tooltip_text(Some(&format!("Install {}", app.name)));
            get.set_sensitive(!ui().busy.get());
            let a = (*app).clone();
            get.connect_clicked(move |_| install(&a));
            get.upcast()
        };
        action.set_valign(gtk::Align::Center);
        actions.append(&action);
        let source = widgets::label(&source_label(app), "feature-source");
        source.set_valign(gtk::Align::Center);
        actions.append(&source);
        words.append(&actions);
        hero.add_overlay(&words);
        hero.set_measure_overlay(&words, true);
        let click = gtk::GestureClick::new();
        let a = (*app).clone();
        click.connect_released(move |gesture, _, _, _| {
            gesture.set_state(gtk::EventSequenceState::Claimed);
            details(a.clone());
        });
        hero.add_controller(click);
        stack.add_named(&hero, Some(&index.to_string()));
    }
    let spotlight = gtk::Overlay::new();
    spotlight.add_css_class("spotlight");
    spotlight.set_child(Some(&stack));
    if scenes.len() > 1 {
        let dots = widgets::hbox(5);
        dots.add_css_class("page-dots");
        dots.set_halign(gtk::Align::Start);
        dots.set_valign(gtk::Align::End);
        let marks: Rc<RefCell<Vec<gtk::Button>>> = Rc::new(RefCell::new(Vec::new()));
        for (index, (app, ..)) in scenes.iter().enumerate() {
            let dot = gtk::Button::new();
            dot.add_css_class("page-dot");
            if index == 0 {
                dot.add_css_class("current");
            }
            dot.set_tooltip_text(Some(&app.name));
            dot.update_property(&[gtk::accessible::Property::Label(&format!("Show {}", app.name))]);
            let (stack, marks_ref) = (stack.clone(), marks.clone());
            dot.connect_clicked(move |_| {
                for (i, m) in marks_ref.borrow().iter().enumerate() {
                    if i == index {
                        m.add_css_class("current");
                    } else {
                        m.remove_css_class("current");
                    }
                }
                stack.set_visible_child_name(&index.to_string());
            });
            dots.append(&dot);
            marks.borrow_mut().push(dot);
        }
        spotlight.add_overlay(&dots);
    }
    ui().body.append(&spotlight);
}
