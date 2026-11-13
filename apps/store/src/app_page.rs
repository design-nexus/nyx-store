//! An app's own page, opened over the page it came from: its icon, name and maker,
//! where it comes from and who it installs for, Install or Open, its screenshots, its
//! description and the facts, with the command that installs it from a terminal.
use super::*;
use nyx_store::nix::{FLATHUB, NIXPKGS, NYXOS};

pub(super) fn render(c: &Catalog, app: &App) {
    let u = ui();
    let narrow = u.tier.get() == 0;
    let page = widgets::vbox(0);
    page.add_css_class("store-app-page");

    let from = place_name(Some(c), &u.page.borrow(), &u.category.borrow());
    let back = gtk::Button::new();
    back.add_css_class("back-link");
    back.set_halign(gtk::Align::Start);
    let row = widgets::hbox(4);
    row.append(&gtk::Image::from_icon_name("go-previous-symbolic"));
    row.append(&gtk::Label::new(Some(&from)));
    back.set_child(Some(&row));
    back.set_tooltip_text(Some(&format!("Back to {from} (Esc)")));
    back.update_property(&[gtk::accessible::Property::Label(&format!("Back to {from}"))]);
    back.connect_clicked(|_| close_details());
    page.append(&back);

    page.append(&head(c, app));
    if app.source == "aur" {
        let banner = widgets::banner(
            "AUR packages are community build recipes. Review the files before building. Builds run as your user.",
            true,
        );
        banner.set_margin_top(18);
        page.append(&banner);
    }
    page.append(&screenshots(app, narrow));

    let columns = gtk::Box::new(if narrow { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal }, 22);
    columns.add_css_class("app-columns");
    let words = widgets::vbox(10);
    words.set_hexpand(true);
    let description = if app.description.is_empty() { &app.summary } else { &app.description };
    for paragraph in description.split("\n\n").filter(|p| !p.trim().is_empty()) {
        // AppStream's paragraphs keep the XML's line breaks and indents.
        let paragraph = paragraph.split_whitespace().collect::<Vec<_>>().join(" ");
        let label = widgets::label(&paragraph, "app-description");
        label.set_wrap(true);
        label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
        label.set_selectable(true);
        // Focused, a selectable label selects all its text; it's still selected by dragging.
        label.set_focusable(false);
        words.append(&label);
    }
    for (source, package) in &app.also {
        let Some(other) = c.apps.iter().find(|a| &a.source == source && &a.package == package).cloned() else { continue };
        let (row, _) = widgets::button_row(
            &format!("Also on {}", source_name(source)),
            &if other.installed.is_some() { format!("{} · installed", other.name) } else { other.name.clone() },
            "Show",
            move |_| details(other.clone()),
        );
        row.add_css_class("card");
        words.append(&row);
    }
    columns.append(&words);
    let info = facts(app);
    info.set_valign(gtk::Align::Start);
    if !narrow {
        info.set_size_request(330, -1);
    }
    columns.append(&info);
    page.append(&columns);
    u.body.append(&nyx_ui::clamp::Clamp::new(&page, 900));
}

/// The icon, the name, who makes it and what it is, the chips, and the action.
fn head(c: &Catalog, app: &App) -> gtk::Box {
    let head = widgets::hbox(20);
    head.add_css_class("app-head");
    let icon = app_icon(app, 88);
    icon.set_valign(gtk::Align::Center);
    head.append(&icon);
    let words = widgets::vbox(3);
    words.set_hexpand(true);
    words.set_valign(gtk::Align::Center);
    let name = widgets::label(&app.name, "app-page-name");
    name.set_wrap(true);
    words.append(&name);
    let about = if app.summary.is_empty() { app.category.as_str() } else { app.summary.as_str() };
    let by: Vec<&str> = [app.developer.as_str(), about].into_iter().filter(|s| !s.is_empty()).collect();
    if !by.is_empty() {
        let line = widgets::label(&by.join(" · "), "app-by");
        line.set_wrap(true);
        words.append(&line);
    }
    let chips = widgets::hbox(6);
    chips.add_css_class("app-chips");
    chips.set_margin_top(6);
    chips.append(&chip(&source_label(app), Some("network-server-symbolic"), true));
    if !app.license.is_empty() {
        chips.append(&chip(&app.license, None, false));
    }
    if let Some(scope) = scope(app) {
        chips.append(&chip(scope, Some("system-users-symbolic"), false));
    }
    words.append(&chips);
    head.append(&words);
    head.append(&actions(c, app));
    head
}

/// A small rounded label with an optional icon, in the accent or neutral.
fn chip(text: &str, icon: Option<&str>, accent: bool) -> gtk::Box {
    let chip = widgets::hbox(5);
    chip.add_css_class("app-chip");
    if accent {
        chip.add_css_class("accent");
    }
    if let Some(icon) = icon {
        let image = gtk::Image::from_icon_name(icon);
        image.set_pixel_size(14);
        chip.append(&image);
    }
    let label = widgets::label(text, "");
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_max_width_chars(32);
    label.set_tooltip_text(Some(text));
    chip.append(&label);
    chip
}

/// Who an install is for: the system's apps are everyone's, Flathub's and NyxOS's
/// own-scope apps are yours.
fn scope(app: &App) -> Option<&'static str> {
    if app.built_in {
        return Some("Built into NyxOS");
    }
    if nyx_store::nix::active() || app.suite_name().is_some() {
        return match app.scope.as_str() {
            "everyone" => Some("Installs for everyone"),
            "you" => Some("Installs for you"),
            _ => (app.source == FLATHUB).then_some("Installs for you"),
        };
    }
    match app.source.as_str() {
        "unknown" | "local" | "" => None,
        _ => Some("Installs for everyone"),
    }
}

/// Install (or Review build), with what it will ask; Open and Remove once installed, and
/// on Arch Update for a Nyx app.
fn actions(c: &Catalog, app: &App) -> gtk::Box {
    let act = widgets::vbox(5);
    act.add_css_class("app-actions");
    act.set_valign(gtk::Align::Center);
    let buttons = widgets::hbox(8);
    buttons.set_halign(gtk::Align::Center);
    let note = if app.installed.is_some() {
        if !app.desktop_id.is_empty() {
            let open = gtk::Button::with_label("Open");
            open.add_css_class("suggested-action");
            open.add_css_class("big-action");
            let a = app.clone();
            open.connect_clicked(move |_| launch(&a));
            buttons.append(&open);
        }
        if let Some(name) = app.suite_name() {
            if c.suite.install.contains(name) {
                let name = name.to_string();
                let update = gtk::Button::with_label("Update");
                update.add_css_class("big-action");
                update.set_tooltip_text(Some(&format!("Install the latest release of {}", app.name)));
                update.set_sensitive(!ui().busy.get());
                update.connect_clicked(move |_| {
                    start_request(Request::NyxApps { change: "update".into(), apps: vec![name.clone()] })
                });
                buttons.append(&update);
            }
            if app.removable() && c.suite.remove.contains(name) {
                let name = name.to_string();
                let remove = widgets::two_click("Remove", "Click again to remove", move || {
                    start_request(Request::NyxApps { change: "remove".into(), apps: vec![name.clone()] });
                });
                remove.add_css_class("big-action");
                remove.set_sensitive(!ui().busy.get());
                buttons.append(&remove);
            }
        } else if app.removable() {
            let package = app.package.clone();
            let remove = widgets::two_click("Remove", "Click again to remove", move || {
                start_request(Request::Remove { package: package.clone() });
            });
            remove.add_css_class("big-action");
            remove.set_sensitive(!ui().busy.get());
            buttons.append(&remove);
        }
        match (&app.installed, app.built_in) {
            (_, true) => "Built into NyxOS".to_string(),
            (Some(v), _) if !v.is_empty() => format!("Installed · {v}"),
            _ => "Installed".to_string(),
        }
    } else {
        let get = gtk::Button::new();
        get.add_css_class("suggested-action");
        get.add_css_class("big-action");
        let content = widgets::hbox(8);
        content.append(&gtk::Image::from_icon_name("folder-download-symbolic"));
        content.append(&gtk::Label::new(Some(if app.source == "aur" { "Review build" } else { "Install" })));
        get.set_child(Some(&content));
        get.set_sensitive(!ui().busy.get());
        let a = app.clone();
        get.connect_clicked(move |_| install(&a));
        buttons.append(&get);
        asks(app).to_string()
    };
    if buttons.first_child().is_some() {
        act.append(&buttons);
    }
    let note = widgets::label(&note, "action-note");
    note.set_halign(gtk::Align::Center);
    note.set_justify(gtk::Justification::Center);
    note.set_wrap(true);
    note.set_max_width_chars(26);
    act.append(&note);
    act
}

/// What installing will ask for.
fn asks(app: &App) -> &'static str {
    if app.source == "aur" {
        return "You review its recipe, then it builds as you";
    }
    if app.suite_name().is_some() {
        return "Installs for you; no password needed";
    }
    if nyx_store::nix::active() {
        return if app.scope == "everyone" { "Asks for an administrator's password" } else { "No password needed" };
    }
    "Asks for your password"
}

/// Up to three screenshots, the first larger. They load in the background; any that
/// can't load (or turn out to be the same picture) drop out, and the rest close up.
fn screenshots(app: &App, narrow: bool) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.add_css_class("app-shots");
    grid.set_column_homogeneous(true);
    grid.set_column_spacing(12);
    grid.set_height_request(if narrow { 170 } else { 220 });
    let pictures: Vec<gtk::Picture> = (0..3)
        .map(|_| {
            let picture = gtk::Picture::new();
            picture.add_css_class("app-shot");
            picture.set_can_shrink(true);
            picture.set_content_fit(gtk::ContentFit::Cover);
            picture.set_overflow(gtk::Overflow::Hidden);
            picture.set_vexpand(true);
            picture
        })
        .collect();
    // Each picture: still loading (None), loaded from a file, or gone.
    let states: Rc<RefCell<Vec<Option<Option<std::path::PathBuf>>>>> = Rc::new(RefCell::new(vec![None; 3]));
    let layout = {
        let (grid, pictures, states) = (grid.clone(), pictures.clone(), states.clone());
        Rc::new(move || {
            for p in &pictures {
                if p.parent().is_some() {
                    grid.remove(p);
                }
            }
            let shown: Vec<&gtk::Picture> =
                pictures.iter().zip(states.borrow().iter()).filter(|(_, s)| !matches!(s, Some(None))).map(|(p, _)| p).collect();
            // Seventeen columns: 7 + 5 + 5 for three, 9 + 8 for two.
            let spans: &[i32] = match shown.len() {
                3 => &[7, 5, 5],
                2 => &[9, 8],
                _ => &[17],
            };
            let mut column = 0;
            for (picture, span) in shown.iter().zip(spans) {
                grid.attach(*picture, column, 0, *span, 1);
                column += span;
            }
            grid.set_visible(!shown.is_empty());
        })
    };
    layout();
    let cache = nyx_core::paths::cache_dir();
    for (index, picture) in pictures.into_iter().enumerate() {
        let (app, cache) = (app.clone(), cache.clone());
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(nyx_store::assets::screenshot(&cache, &app, index));
        });
        let (states, layout) = (states.clone(), layout.clone());
        glib::spawn_future_local(async move {
            let Ok(result) = rx.recv().await else { return };
            let texture = result.ok().filter(|path| !states.borrow().iter().any(|s| s.as_ref().is_some_and(|p| p.as_ref() == Some(path))))
                .and_then(|path| gdk::Texture::from_filename(&path).ok().map(|t| (path, t)));
            match texture {
                Some((path, texture)) => {
                    picture.set_paintable(Some(&texture));
                    states.borrow_mut()[index] = Some(Some(path));
                }
                None => states.borrow_mut()[index] = Some(None),
            }
            layout();
        });
    }
    grid
}

/// The facts: version, download size, package, website, and the terminal command.
fn facts(app: &App) -> gtk::Box {
    let card = widgets::vbox(0);
    card.add_css_class("card");
    card.add_css_class("app-facts");
    let row = |key: &str, value: &gtk::Widget| {
        let row = widgets::hbox(10);
        row.add_css_class("fact");
        let k = widgets::label(key, "fact-key");
        k.set_hexpand(true);
        row.append(&k);
        value.set_valign(gtk::Align::Center);
        row.append(value);
        card.append(&row);
        row
    };
    let value = |text: &str| {
        let l = widgets::label(text, "fact-value");
        l.set_ellipsize(gtk::pango::EllipsizeMode::End);
        l.set_max_width_chars(28);
        l.set_xalign(1.0);
        l.upcast::<gtk::Widget>()
    };
    if !app.version.is_empty() {
        row("Version", &value(&app.version));
    }
    if let Some(installed) = app.installed.as_deref().filter(|v| !v.is_empty() && *v != app.version) {
        row("Installed", &value(installed));
    }
    if app.download_size > 0 {
        row("Download", &value(&size(app.download_size)));
    }
    row("Package", &value(&app.package));
    if app.website.starts_with("https://") {
        let host = app.website.trim_start_matches("https://").trim_start_matches("www.").trim_end_matches('/');
        let link = gtk::LinkButton::with_label(&app.website, host);
        link.add_css_class("fact-link");
        link.set_tooltip_text(Some(&app.website));
        if let Some(label) = link.child().and_downcast::<gtk::Label>() {
            label.set_ellipsize(gtk::pango::EllipsizeMode::End);
            label.set_max_width_chars(28);
        }
        row("Website", link.upcast_ref());
    }
    if let Some(command) = command(app) {
        let line = widgets::hbox(6);
        let text = widgets::label(&command, "fact-command");
        text.add_css_class("mono");
        text.set_selectable(true);
        text.set_focusable(false);
        text.set_ellipsize(gtk::pango::EllipsizeMode::End);
        line.append(&text);
        let copy = widgets::icon_button("edit-copy-symbolic", "Copy the command");
        copy.connect_clicked(move |b| {
            b.clipboard().set_text(&command);
            toast("Copied the install command");
        });
        line.append(&copy);
        row("From a terminal", line.upcast_ref()).add_css_class("command");
    }
    card
}

/// The command that installs the app from a terminal, where there is one.
fn command(app: &App) -> Option<String> {
    if app.built_in || app.installed.is_some() {
        return None;
    }
    Some(match app.source.as_str() {
        NYXOS | NIXPKGS => format!("nyx install {}", app.package),
        FLATHUB => format!("flatpak install --user flathub {}", app.package),
        "aur" => format!("yay -S {}", app.package),
        nyx_store::suite::SUITE => nyx_store::suite::command("install", &[app.suite_name()?.to_string()]),
        "core" | "extra" | "multilib" | "omarchy" => format!("sudo pacman -S {}", app.package),
        "unknown" | "local" | "" => return None,
        repository => format!("sudo pacman -S {repository}/{}", app.package),
    })
}

/// A download size as people read it: "112 MB".
fn size(bytes: i64) -> String {
    let mb = bytes as f64 / 1_000_000.0;
    if mb >= 10.0 {
        format!("{mb:.0} MB")
    } else if mb >= 1.0 {
        format!("{mb:.1} MB")
    } else {
        format!("{} kB", (bytes as f64 / 1000.0).ceil())
    }
}
