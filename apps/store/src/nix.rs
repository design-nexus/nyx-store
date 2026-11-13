//! NixOS (NyxOS): three sources. The NyxOS software catalog (Nyx apps and curated
//! software, shared with Nyx Welcome) and nixpkgs install for everyone through the Nyx
//! System Manager (`nyx-system`), as one rebuild that's only switched to when it builds;
//! Flathub apps install for the user. Updates are the system's (NyxOS, its Nyx apps and
//! everything installed for everyone, together) and Flathub's, and a system change can
//! be undone by going back a generation.
//!
//! Requests and events are the pacman helper's, so the window shows the same review
//! before anything changes; the review lists what has to be agreed to. Packages an older
//! Store put in the user's `nix profile` still show, and can be updated and removed.

use crate::{
    catalog::{self, Catalog},
    model::{App, Change, Event, Installed, Plan, Request, valid_name},
};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use nyx_catalog::{Catalog as Software, Installed as Have, Item, Tier};
use nyx_system_proto::{Change as SystemChange, SystemSettings};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const FLATHUB: &str = "flathub";
pub const NIXPKGS: &str = "nixpkgs";
/// The NyxOS software catalog.
pub const NYXOS: &str = "nyxos";
const FLATHUB_REPO: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";
const SYSTEM: &str = "legacyPackages.x86_64-linux.";
const PROFILES: &str = "/nix/var/nix/profiles";

/// Flathub apps whose descriptions and artwork nixpkgs packages share, by attribute.
const ART: &[(&str, &str)] = &[
    ("firefox", "org.mozilla.firefox"),
    ("chromium", "org.chromium.Chromium"),
    ("brave", "com.brave.Browser"),
    ("vivaldi", "com.vivaldi.Vivaldi"),
    ("thunderbird", "org.mozilla.Thunderbird"),
    ("signal-desktop", "org.signal.Signal"),
    ("telegram-desktop", "org.telegram.desktop"),
    ("discord", "com.discordapp.Discord"),
    ("transmission_4-gtk", "com.transmissionbt.Transmission"),
    ("libreoffice-fresh", "org.libreoffice.LibreOffice"),
    ("onlyoffice-desktopeditors", "org.onlyoffice.desktopeditors"),
    ("keepassxc", "org.keepassxc.KeePassXC"),
    ("calibre", "com.calibre_ebook.calibre"),
    ("gimp", "org.gimp.GIMP"),
    ("inkscape", "org.inkscape.Inkscape"),
    ("krita", "org.kde.krita"),
    ("blender", "org.blender.Blender"),
    ("darktable", "org.darktable.Darktable"),
    ("vlc", "org.videolan.VLC"),
    ("obs-studio", "com.obsproject.Studio"),
    ("audacity", "org.audacityteam.Audacity"),
    ("ardour", "org.ardour.Ardour"),
    ("kdePackages.kdenlive", "org.kde.kdenlive"),
    ("shotcut", "org.shotcut.Shotcut"),
    ("zed-editor", "dev.zed.Zed"),
    ("vscode", "com.visualstudio.code"),
    ("vscodium", "com.vscodium.codium"),
    ("godot", "org.godotengine.Godot"),
    ("lutris", "net.lutris.Lutris"),
    ("heroic", "com.heroicgameslauncher.hgl"),
    ("superTuxKart", "net.supertuxkart.SuperTuxKart"),
    ("gnome-boxes", "org.gnome.Boxes"),
    ("remmina", "org.remmina.Remmina"),
    ("rustdesk-flutter", "com.rustdesk.RustDesk"),
    ("virt-manager", "org.virt_manager.virt-manager"),
    ("stellarium", "org.stellarium.Stellarium"),
    ("wireshark", "org.wireshark.Wireshark"),
];

/// nixpkgs picks the catalog doesn't have: attribute and category.
const PICKS: &[(&str, &str)] = &[
    ("thunderbird", "Internet"),
    ("transmission_4-gtk", "Internet"),
    ("keepassxc", "Productivity"),
    ("calibre", "Productivity"),
    ("blender", "Graphics"),
    ("darktable", "Graphics"),
    ("vlc", "Audio & video"),
    ("obs-studio", "Audio & video"),
    ("godot", "Development"),
    ("superTuxKart", "Games"),
    ("stellarium", "Education"),
    ("wireshark", "Security"),
];

/// Whether Store runs on NixOS, where these sources take pacman's place.
pub fn active() -> bool {
    Path::new("/etc/NIXOS").exists()
}

/// Store's category for a catalog category.
pub(crate) fn category(id: &str) -> &'static str {
    match id {
        "browsers" | "chat" | "remote" | "nyx-communication" => "Internet",
        "office" | "nyx-productivity" | "nyx-essentials" | "nyx-tools" => "Productivity",
        "graphics" => "Graphics",
        "video" | "audio" | "nyx-media" => "Audio & video",
        "games" => "Games",
        _ => "Development",
    }
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
}

fn appstream_dir() -> PathBuf {
    data_home().join("flatpak/appstream").join(FLATHUB).join("x86_64/active")
}

fn command(program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program).args(args).stdin(Stdio::null()).output().with_context(|| format!("{program} is missing"))?;
    ensure!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr).trim());
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Flathub's remote and AppStream data for this user, fetched when missing or asked.
fn flathub_appstream(refresh: bool) -> Result<String> {
    command("flatpak", &["remote-add", "--user", "--if-not-exists", FLATHUB, FLATHUB_REPO])?;
    let dir = appstream_dir();
    let (gz, xml) = (dir.join("appstream.xml.gz"), dir.join("appstream.xml"));
    if refresh || !(gz.is_file() || xml.is_file()) {
        command("flatpak", &["update", "--user", "--appstream", FLATHUB])?;
    }
    let mut text = String::new();
    if gz.is_file() {
        GzDecoder::new(fs::File::open(&gz)?).take(256 * 1024 * 1024).read_to_string(&mut text)?;
    } else {
        text = fs::read_to_string(&xml)?;
    }
    Ok(text)
}

/// `/nix/store/HASH-firefox-144.0` → `144.0`.
fn store_version(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or("").split_once('-').map(|(_, n)| n).unwrap_or("");
    name.match_indices('-')
        .find(|(i, _)| name[i + 1..].starts_with(|c: char| c.is_ascii_digit()))
        .map(|(i, _)| name[i + 1..].to_string())
        .unwrap_or_default()
}

/// nixpkgs attributes in the user's profile (from an older Store), with the element
/// names `nix profile` uses.
fn profile() -> BTreeMap<String, (String, Installed)> {
    let Ok(text) = command("nix", &["profile", "list", "--json"]) else { return BTreeMap::new() };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let mut out = BTreeMap::new();
    for (element, e) in v["elements"].as_object().into_iter().flatten() {
        let Some(attr) = e["attrPath"].as_str().and_then(|a| a.strip_prefix(SYSTEM)) else { continue };
        // nixpkgs, or the system's channel (what apps from nixpkgs install from now).
        if !e["originalUrl"].as_str().is_some_and(|u| u.contains(NIXPKGS) || u.starts_with("github:nyxos-project/nyxos")) {
            continue;
        }
        let path = e["storePaths"][0].as_str().unwrap_or("");
        let version = store_version(path);
        out.insert(attr.to_string(), (element.clone(), Installed { name: attr.into(), version, stamp: path.into() }));
    }
    out
}

fn flatpaks() -> BTreeMap<String, Installed> {
    let text = command("flatpak", &["list", "--user", "--app", "--columns=application,version"]).unwrap_or_default();
    text.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim();
            (!id.is_empty()).then(|| {
                (
                    id.to_string(),
                    Installed { name: id.into(), version: f.next().unwrap_or("").trim().into(), stamp: String::new() },
                )
            })
        })
        .collect()
}

/// The System Manager's settings (through `nyx-system`; empty when it can't be reached).
fn system_settings() -> SystemSettings {
    command("nyx-system", &["settings"]).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn software() -> Result<Software> {
    Software::load()
}

/// What an item installs and needs, for its details.
fn requirements(item: &Item) -> String {
    let mut lines = Vec::new();
    let everyone = item.scope() == nyx_catalog::Scope::System;
    if !item.nyx.is_empty() {
        lines.push(
            if everyone { "A Nyx app built into the desktop, installed for everyone." } else { "A Nyx app, installed for you." }
                .to_string(),
        );
    }
    if !item.packages.is_empty() {
        lines.push(if everyone {
            format!("Installs for everyone (nixpkgs): {}.", item.packages.join(", "))
        } else {
            format!("Installs for you, into your Nix profile: {}.", item.packages.join(", "))
        });
    }
    if let Some(f) = &item.flatpak {
        lines.push(format!("Installs for you from Flathub ({f})."));
    }
    if let Some(f) = &item.profile {
        lines.push(format!("Installs for you into your Nix profile, from {f}."));
    }
    if !item.services.is_empty() {
        lines.push(format!("Turns on the system service {}.", item.services.join(", ")));
    }
    if !item.groups.is_empty() {
        lines.push(format!("Adds you to the {} group; you're asked first.", item.groups.join(", ")));
    }
    if let Some(note) = &item.note {
        lines.push(note.clone());
    }
    format!("{}\n\n{}", item.summary, lines.join("\n"))
}

pub fn load(cache: &Path, refresh: bool) -> Result<Catalog> {
    fs::create_dir_all(cache)?;
    let (mut flathub, warning) = match flathub_appstream(refresh).and_then(|x| catalog::parse(&x)) {
        Ok(apps) => (apps, String::new()),
        Err(e) => (Vec::new(), format!("Flathub could not load: {e}")),
    };
    // Icons resolve as @cache:flathub/SIZE/ID.png under cache/swicons.
    let icons = cache.join("swicons");
    fs::create_dir_all(&icons)?;
    let link = icons.join(FLATHUB);
    if fs::read_link(&link).ok() != Some(appstream_dir().join("icons")) {
        let _ = fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(appstream_dir().join("icons"), &link);
    }
    let by_id: BTreeMap<String, App> = flathub.iter().map(|a| (a.package.clone(), a.clone())).collect();
    let art = |attr: &str| ART.iter().find(|(a, _)| *a == attr).and_then(|(_, id)| by_id.get(*id));

    let settings = system_settings();
    let have = Have::probe(settings.clone());
    let sw = software()?;
    let groups = groups(&sw);
    let mut apps = Vec::new();
    let mut installed = BTreeMap::new();
    for item in sw.items.iter().filter(|i| i.tier != Tier::Base || !i.nyx.is_empty()) {
        let looks = item.flatpak.as_deref().and_then(|f| by_id.get(f)).or_else(|| item.packages.first().and_then(|p| art(p)));
        let mut a = looks.cloned().unwrap_or_default();
        let group = groups.iter().find(|g| g.id == item.category).cloned().unwrap_or_default();
        a.package = item.id.clone();
        a.name = item.name.clone();
        a.summary = item.summary.clone();
        a.description = requirements(item);
        a.category = category(&item.category).into();
        a.source = NYXOS.into();
        a.license = if item.unfree { "Not free: you accept its license when installing".into() } else { a.license };
        a.popular = item.tier == Tier::Welcome && looks.is_some();
        a.desktop = desktop(&group);
        a.nyx = !item.nyx.is_empty();
        a.built_in = item.tier == Tier::Base;
        a.scope = if item.scope() == nyx_catalog::Scope::System { "everyone" } else { "you" }.into();
        a.keywords = item.names();
        a.also = looks.map(|l| vec![(FLATHUB.to_string(), l.package.clone())]).unwrap_or_default();
        a.also.extend(item.packages.iter().map(|p| (NIXPKGS.to_string(), p.clone())));
        if let Some(name) = item.nyx.first().filter(|_| looks.is_none()) {
            let id = nyx_app_id(name);
            a.icon = id.clone();
            a.desktop_id = format!("{id}.desktop");
        }
        a.group = (group.id.clone(), group.name.clone());
        a.section = group.section.clone();
        if a.built_in || have.has(item) {
            installed.insert(item.id.clone(), Installed { name: item.id.clone(), version: String::new(), stamp: String::new() });
        }
        apps.push(a);
    }
    for &(attr, category) in PICKS {
        let looks = art(attr);
        let mut a = looks.cloned().unwrap_or_else(|| App { name: attr.into(), desktop: true, ..Default::default() });
        a.package = attr.into();
        a.source = NIXPKGS.into();
        a.category = category.into();
        a.popular = true;
        a.scope = "you".into();
        a.keywords = vec![attr.to_lowercase()];
        a.also = looks.map(|l| vec![(FLATHUB.to_string(), l.package.clone())]).unwrap_or_default();
        apps.push(a);
    }
    for a in &mut flathub {
        a.source = FLATHUB.into();
        a.popular = ART.iter().any(|(_, id)| *id == a.package);
        a.scope = "you".into();
    }
    apps.extend(flathub);

    for attr in &settings.packages {
        installed.insert(attr.clone(), Installed { name: attr.clone(), version: String::new(), stamp: String::new() });
    }
    for (attr, (_, i)) in profile() {
        installed.insert(attr, i);
    }
    installed.extend(flatpaks());
    for a in &mut apps {
        a.installed = installed.get(&a.package).map(|i| i.version.clone());
    }
    let updates = command("flatpak", &["remote-ls", "--user", "--updates", "--app", "--columns=application,version", FLATHUB])
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.trim().to_string();
            let old = installed.get(&id).map(|i| i.version.clone());
            Some(Change { name: id, old, new: f.next().map(|v| v.trim().to_string()), source: FLATHUB.into(), bytes: 0 })
        })
        .collect();
    Ok(Catalog {
        apps,
        installed,
        sources: BTreeMap::new(),
        candidates: BTreeMap::new(),
        // The source menu lists these.
        repositories: [NYXOS, NIXPKGS, FLATHUB]
            .map(|name| crate::repositories::Repository { name: name.into(), servers: Vec::new(), enabled: true })
            .into(),
        updates,
        warning,
        groups,
        bundles: sw
            .presets
            .iter()
            .map(|p| catalog::Bundle {
                id: p.id.clone(),
                name: p.name.clone(),
                summary: p.summary.clone(),
                items: p.items.clone(),
                ..Default::default()
            })
            .collect(),
        suite: Default::default(),
    })
}

/// The catalog's categories in its order, each with its section's id and name.
pub(crate) fn groups(sw: &Software) -> Vec<catalog::Group> {
    sw.categories
        .iter()
        .map(|c| catalog::Group {
            id: c.id.clone(),
            name: c.name.clone(),
            section: (
                c.section.clone(),
                sw.sections.iter().find(|s| s.id == c.section).map(|s| s.name.clone()).unwrap_or_default(),
            ),
        })
        .collect()
}

/// Whether a catalog category holds apps with a window, rather than terminal tools,
/// toolchains, services or coding agents.
fn desktop(group: &catalog::Group) -> bool {
    !matches!(group.section.0.as_str(), "cli" | "toolchains" | "infrastructure")
        && !matches!(group.id.as_str(), "coding-agents" | "local-models")
}

/// A Nyx app's application ID, which names its launcher and icon (`media-player` is
/// `io.github.design_nexus.nyx.MediaPlayer`).
pub(crate) fn nyx_app_id(name: &str) -> String {
    let camel: String = name
        .split('-')
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default()
        })
        .collect();
    format!("io.github.design_nexus.nyx.{camel}")
}

/// What `nyx update`'s last check found (`$XDG_CACHE_HOME/nyx/update-status.json`, kept
/// by nyx-system): when it ran, in seconds since the epoch, and each update but firmware.
pub fn checked_updates() -> Option<(i64, Vec<Change>)> {
    let cache = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".cache"));
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(cache.join("nyx/update-status.json")).ok()?).ok()?;
    let text = |v: &serde_json::Value| v.as_str().map(String::from);
    let items = v["status"]["available"]
        .as_array()?
        .iter()
        .filter(|i| !i["how"].as_str().unwrap_or("").starts_with("firmware"))
        .map(|i| Change {
            name: text(&i["name"]).unwrap_or_default(),
            old: text(&i["from"]),
            new: text(&i["to"]),
            source: text(&i["how"]).unwrap_or_default(),
            bytes: 0,
        })
        .collect();
    Some((v["time"].as_i64().unwrap_or(0), items))
}

/// nixpkgs packages matching `query` (`nix search`), for anything not in the catalog.
pub fn search(query: &str) -> Result<Vec<App>> {
    let words: Vec<&str> = query.split_whitespace().filter(|w| w.len() > 1).take(4).collect();
    ensure!(!words.is_empty(), "Type a longer search");
    let mut args = vec!["search", NIXPKGS, "--json"];
    args.extend(&words);
    let v: serde_json::Value = serde_json::from_str(&command("nix", &args)?)?;
    let mut apps: Vec<App> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(path, p)| {
            let attr = path.strip_prefix(SYSTEM)?.to_string();
            valid_name(&attr).then(|| App {
                name: p["pname"].as_str().unwrap_or(&attr).to_string(),
                summary: p["description"].as_str().unwrap_or("").to_string(),
                version: p["version"].as_str().unwrap_or("").to_string(),
                package: attr,
                source: NIXPKGS.into(),
                ..Default::default()
            })
        })
        .collect();
    apps.truncate(200);
    Ok(apps)
}

/// Whether nixpkgs marks `attr` unfree (the system's nixpkgs, through the registry).
fn unfree(attr: &str) -> bool {
    command("nix", &["eval", "--json", &format!("{NIXPKGS}#{attr}.meta.unfree")]).is_ok_and(|t| t.trim() == "true")
}

/// The system's generations: how many, the current one and the one before it.
pub fn generations() -> (usize, Option<u32>, Option<u32>) {
    let mut numbers: Vec<u32> = fs::read_dir(PROFILES)
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().to_string_lossy().strip_prefix("system-")?.strip_suffix("-link")?.parse().ok())
                .collect()
        })
        .unwrap_or_default();
    numbers.sort();
    let current: Option<u32> = fs::read_link(Path::new(PROFILES).join("system"))
        .ok()
        .and_then(|l| l.to_string_lossy().strip_prefix("system-")?.strip_suffix("-link")?.parse().ok());
    let previous = current.and_then(|c| numbers.iter().rev().find(|n| **n < c).copied());
    (numbers.len(), current, previous)
}

fn user() -> String {
    std::env::var("USER").unwrap_or_default()
}

fn change(name: &str, new: &str, source: &str) -> Change {
    Change { name: name.into(), old: None, new: Some(new.into()), source: source.into(), bytes: 0 }
}

/// What a request takes: the system's changes, Flathub's, the user's old profile's, and
/// what has to be agreed to. Worked out the same way for the review and for applying.
struct Work {
    system: Vec<SystemChange>,
    flatpak_install: Vec<String>,
    flatpak_remove: Vec<String>,
    profile_remove: Vec<String>,
    /// Catalog flakes for the user's Nix profile.
    flake_install: Vec<String>,
    flake_remove: Vec<String>,
    update: bool,
    rollback: bool,
    plan: Plan,
}

fn work(request: &Request) -> Result<Work> {
    let mut w = Work {
        system: Vec::new(),
        flatpak_install: Vec::new(),
        flatpak_remove: Vec::new(),
        profile_remove: Vec::new(),
        flake_install: Vec::new(),
        flake_remove: Vec::new(),
        update: false,
        rollback: false,
        plan: Plan::default(),
    };
    let settings = system_settings();
    match request {
        Request::Install { source, packages } => {
            ensure!(!packages.is_empty() && packages.iter().all(|p| valid_name(p)), "Invalid package selection");
            match source.as_str() {
                NYXOS => {
                    let sw = software()?;
                    let ids: BTreeSet<String> = packages.iter().cloned().collect();
                    let have = Have::probe(settings);
                    let p = sw.plan(&ids, &user(), &have)?;
                    for id in &ids {
                        if let Some(i) = sw.item(id).filter(|i| !have.has(i)) {
                            let how = if i.flatpak.is_some() {
                                "for you, from Flathub"
                            } else if i.scope() == nyx_catalog::Scope::User {
                                "for you, into your Nix profile"
                            } else {
                                "for everyone"
                            };
                            w.plan.changes.push(change(&i.name, how, NYXOS));
                        }
                    }
                    w.plan.notes = p.consent.iter().map(|c| c.describe()).collect();
                    w.system = p.changes;
                    w.flatpak_install = p.flatpaks;
                    w.flake_install = p.profiles;
                }
                // Apps from nixpkgs are the person's own, from the system's channel; an
                // unfree one the catalog doesn't offer is installed for everyone, with its
                // license accepted (per-user installs allow only the catalog's).
                NIXPKGS => {
                    let unfree: Vec<String> = packages.iter().filter(|p| unfree(p)).cloned().collect();
                    let have = Have::probe(settings);
                    for p in packages {
                        if unfree.contains(p) {
                            w.plan.changes.push(change(p, "for everyone", NIXPKGS));
                        } else {
                            w.plan.changes.push(change(p, "for you, into your Nix profile", NIXPKGS));
                            w.flake_install.push(have.reference(p));
                        }
                    }
                    for u in &unfree {
                        w.plan.notes.push(format!("{u} has a license that isn't free; installing it accepts that license"));
                    }
                    if !unfree.is_empty() {
                        w.system.push(SystemChange::AddPackages { packages: unfree.clone(), unfree });
                    }
                }
                FLATHUB => {
                    for p in packages {
                        w.plan.changes.push(change(p, "for you", FLATHUB));
                    }
                    w.flatpak_install = packages.clone();
                }
                _ => bail!("Install from the NyxOS catalog, nixpkgs or Flathub"),
            }
        }
        Request::Remove { package } => {
            ensure!(valid_name(package), "Invalid package name");
            let sw = software()?;
            let removed = Change { name: package.clone(), old: Some(String::new()), new: None, source: String::new(), bytes: 0 };
            if let Some(item) = sw.item(package) {
                ensure!(item.tier != Tier::Base, "{} is part of NyxOS", item.name);
                let p = sw.removal(&BTreeSet::from([package.clone()]), &Have::probe(settings))?;
                w.system = p.changes;
                w.flatpak_remove = p.flatpaks;
                w.flake_remove = p.profiles;
                w.plan.changes.push(Change { name: item.name.clone(), source: NYXOS.into(), ..removed });
            } else if settings.packages.contains(package) {
                w.system.push(SystemChange::RemovePackages { packages: vec![package.clone()] });
                w.plan.changes.push(Change { source: NIXPKGS.into(), ..removed });
            } else if let Some((element, _)) = profile().get(package) {
                w.profile_remove.push(element.clone());
                w.plan.changes.push(Change { source: "your nix profile".into(), ..removed });
            } else {
                w.flatpak_remove.push(package.clone());
                w.plan.changes.push(Change { source: FLATHUB.into(), ..removed });
            }
        }
        Request::Update => {
            // `nyx update`'s own plan: the system, coding agents, Flatpaks and the nix
            // profile. Firmware isn't updated from here.
            w.update = true;
            let plan: serde_json::Value = serde_json::from_str(&command("nyx-system", &["update", "--dry-run", "--json"])?)?;
            let text = |v: &serde_json::Value| v.as_str().map(String::from);
            for item in plan["items"].as_array().into_iter().flatten() {
                let how = text(&item["how"]).unwrap_or_default();
                if how.starts_with("firmware") {
                    continue;
                }
                w.plan.changes.push(Change {
                    name: text(&item["name"]).unwrap_or_default(),
                    old: text(&item["from"]),
                    new: Some(text(&item["to"]).unwrap_or_else(|| "latest".into())),
                    source: how,
                    bytes: 0,
                });
            }
            w.plan.notes.extend(plan["notes"].as_array().into_iter().flatten().filter_map(text));
            if plan["steps"].as_array().is_some_and(|s| s.iter().any(|s| s["kind"] == "firmware")) {
                w.plan.notes.push("Firmware has an update too: Settings > Software & Updates, or nyx update firmware.".into());
            }
            ensure!(!w.plan.changes.is_empty(), "Everything is up to date");
        }
        Request::Rollback => {
            let (_, current, previous) = generations();
            let previous = previous.context("There's no earlier generation to go back to")?;
            w.rollback = true;
            w.plan.changes.push(Change {
                name: "System".into(),
                old: current.map(|c| format!("generation {c}")),
                new: Some(format!("generation {previous}")),
                source: "NyxOS".into(),
                bytes: 0,
            });
            w.plan.notes.push(
                "Software installed for everyone since then is removed again, and the system settings go back with it. Your files stay as they are."
                    .into(),
            );
        }
        _ => bail!("On NyxOS, Store installs from the NyxOS catalog, nixpkgs and Flathub"),
    }
    Ok(w)
}

/// Run one step, passing its progress lines on as status (and `input` to its stdin).
fn step(send: &async_channel::Sender<Event>, program: &str, args: &[&str], input: Option<&str>) -> Result<()> {
    let _ = send.send_blocking(Event::Status { message: format!("{program} {}", args.join(" ")) });
    let mut child = Command::new(program)
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{program} is missing"))?;
    if let (Some(text), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin.write_all(text.as_bytes())?;
    }
    let stdout = child.stdout.take().unwrap();
    let out_send = send.clone();
    let out = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = out_send.send_blocking(Event::Status { message: line });
        }
    });
    let mut last = String::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            let _ = send.send_blocking(Event::Status { message: line.clone() });
            last = line;
        }
    }
    let _ = out.join();
    ensure!(
        child.wait()?.success(),
        "{}",
        if last.is_empty() { format!("{program} failed") } else { last.trim_start_matches("nyx-system: ").to_string() }
    );
    Ok(())
}

/// `nyx update` without questions: its progress as status, then what came of it (each
/// part that failed, or what finishing it takes).
fn update(send: &async_channel::Sender<Event>) -> Result<String> {
    let args = ["update", "--yes", "--json"];
    let _ = send.send_blocking(Event::Status { message: format!("nyx-system {}", args.join(" ")) });
    let mut child = Command::new("nyx-system")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("nyx-system is missing")?;
    let mut stdout = child.stdout.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let mut last = String::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok) {
        if !line.trim().is_empty() {
            let _ = send.send_blocking(Event::Status { message: line.clone() });
            last = line;
        }
    }
    let text = out.join().unwrap_or_default();
    ensure!(child.wait()?.success(), "{}", last.trim_start_matches("nyx-system: "));
    let outcome: serde_json::Value = serde_json::from_str(&text).context("nyx-system's answer couldn't be read")?;
    let steps = outcome["steps"].as_array().cloned().unwrap_or_default();
    let message = |s: &serde_json::Value| s["message"].as_str().unwrap_or_default().to_string();
    let failed: Vec<String> = steps.iter().filter(|s| s["ok"] != true).map(message).collect();
    if !failed.is_empty() {
        let partly = if failed.len() < steps.len() { "Partly updated. " } else { "" };
        bail!("{partly}{}", failed.join("; "));
    }
    let restart: Vec<&str> = outcome["restart"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
    Ok(if outcome["reboot"] == true {
        "Updated. Restart the computer to finish.".into()
    } else if !restart.is_empty() {
        format!("Updated. Reopen {} to use the new versions.", restart.join(", "))
    } else {
        "Everything is up to date".into()
    })
}

fn apply(w: &Work, send: &async_channel::Sender<Event>) -> Result<String> {
    if w.rollback {
        step(send, "nyx-system", &["rollback"], None)?;
        return Ok("Back to the previous generation".into());
    }
    if w.update {
        return update(send);
    }
    if !w.system.is_empty() {
        step(send, "nyx-system", &["apply", "-"], Some(&serde_json::to_string(&w.system)?))?;
    }
    let line = |l: &str| {
        let _ = send.send_blocking(Event::Status { message: l.to_string() });
    };
    nyx_catalog::flatpak(&w.flatpak_install, true, line)?;
    nyx_catalog::flatpak(&w.flatpak_remove, false, line)?;
    nyx_catalog::nix_profile(&w.flake_install, true, line)?;
    nyx_catalog::nix_profile(&w.flake_remove, false, line)?;
    for element in &w.profile_remove {
        step(send, "nix", &["profile", "remove", element], None)?;
    }
    Ok(if w.plan.changes.iter().all(|c| c.new.is_none()) { "Removed".into() } else { "Installed".into() })
}

/// The window's transaction: a plan to review, then the System Manager, flatpak or nix.
pub fn start(request: Request, send: async_channel::Sender<Event>, confirmation: std::sync::mpsc::Receiver<Option<Plan>>) {
    let result = (|| -> Result<Option<String>> {
        let w = work(&request)?;
        ensure!(!w.plan.changes.is_empty(), "That's already installed");
        let _ = send.send_blocking(Event::Plan { plan: w.plan.clone() });
        match confirmation.recv() {
            Ok(Some(_)) => apply(&w, &send).map(Some),
            _ => Ok(None),
        }
    })();
    let _ = send.send_blocking(match result {
        Ok(Some(message)) => Event::Done { message },
        Ok(None) => return,
        Err(e) => Event::Error { message: format!("{e:#}") },
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_from_store_paths() {
        assert_eq!(store_version("/nix/store/0123abcd-firefox-144.0"), "144.0");
        assert_eq!(store_version("/nix/store/0123abcd-transmission-gtk-4.0.6"), "4.0.6");
        assert_eq!(store_version("/nix/store/0123abcd-hello"), "");
    }

    #[test]
    fn flathub_components_are_named_by_their_ref() {
        let xml = r#"<components origin="flathub"><component type="desktop-application"><id>org.gimp.GIMP</id>
            <name>GNU Image Manipulation Program</name><summary>Create images</summary>
            <bundle type="flatpak">app/org.gimp.GIMP/x86_64/stable</bundle>
            <icon type="cached" width="128" height="128">org.gimp.GIMP.png</icon></component></components>"#;
        let apps = catalog::parse(xml).unwrap();
        assert_eq!(apps[0].package, "org.gimp.GIMP");
        assert_eq!(apps[0].icon, "@cache:flathub/128x128/org.gimp.GIMP.png");
    }

    #[test]
    fn the_catalog_maps_onto_stores_categories() {
        let sw = Software::builtin().unwrap();
        for item in &sw.items {
            assert!(crate::catalog::CATEGORIES.iter().any(|(c, _)| *c == category(&item.category)), "{}", item.id);
            assert!(valid_name(&item.id), "{} isn't a name Store's requests take", item.id);
        }
        for (attr, _) in PICKS {
            assert!(!sw.items.iter().any(|i| i.packages.iter().any(|p| p == attr)), "{attr} is in the catalog too");
        }
        let groups = groups(&sw);
        for item in &sw.items {
            let group = groups.iter().find(|g| g.id == item.category).unwrap_or_else(|| panic!("{} has no category", item.id));
            assert!(!group.section.1.is_empty(), "{}'s section has no name", group.id);
        }
        let of = |id: &str| groups.iter().find(|g| g.id == id).unwrap();
        assert!(desktop(of("browsers")) && desktop(of("nyx-tools")) && desktop(of("agent-apps")));
        assert!(!desktop(of("cli-tools")) && !desktop(of("languages")) && !desktop(of("containers")) && !desktop(of("coding-agents")));
    }

    #[test]
    fn nyx_app_ids() {
        assert_eq!(nyx_app_id("media-player"), "io.github.design_nexus.nyx.MediaPlayer");
        assert_eq!(nyx_app_id("pdf"), "io.github.design_nexus.nyx.Pdf");
    }
}
