//! NixOS: apps from Flathub (Flatpak) and nixpkgs (`nix profile`), both installed into
//! the user's own profile, so nothing needs the root helper. Flathub's AppStream data
//! supplies descriptions, icons and screenshots, for nixpkgs' apps too.
//!
//! Requests and events are the pacman helper's, so the window shows the same review
//! before anything changes.

use crate::{
    catalog::{self, Catalog},
    model::{App, Change, Event, Installed, Plan, Request, valid_name},
};
use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

pub const FLATHUB: &str = "flathub";
pub const NIXPKGS: &str = "nixpkgs";
const FLATHUB_REPO: &str = "https://dl.flathub.org/repo/flathub.flatpakrepo";
const SYSTEM: &str = "legacyPackages.x86_64-linux.";

/// The editorial picks in nixpkgs: attribute, the Flathub app whose description and
/// artwork they share, and the category.
const PICKS: &[(&str, &str, &str)] = &[
    ("firefox", "org.mozilla.firefox", "Internet"),
    ("chromium", "org.chromium.Chromium", "Internet"),
    ("thunderbird", "org.mozilla.Thunderbird", "Internet"),
    ("signal-desktop", "org.signal.Signal", "Internet"),
    ("telegram-desktop", "org.telegram.desktop", "Internet"),
    ("transmission_4-gtk", "com.transmissionbt.Transmission", "Internet"),
    ("libreoffice-fresh", "org.libreoffice.LibreOffice", "Productivity"),
    ("keepassxc", "org.keepassxc.KeePassXC", "Productivity"),
    ("calibre", "com.calibre_ebook.calibre", "Productivity"),
    ("gimp", "org.gimp.GIMP", "Graphics"),
    ("inkscape", "org.inkscape.Inkscape", "Graphics"),
    ("krita", "org.kde.krita", "Graphics"),
    ("blender", "org.blender.Blender", "Graphics"),
    ("darktable", "org.darktable.Darktable", "Graphics"),
    ("vlc", "org.videolan.VLC", "Audio & video"),
    ("obs-studio", "com.obsproject.Studio", "Audio & video"),
    ("audacity", "org.audacityteam.Audacity", "Audio & video"),
    ("kdePackages.kdenlive", "org.kde.kdenlive", "Audio & video"),
    ("zed-editor", "dev.zed.Zed", "Development"),
    ("godot", "org.godotengine.Godot", "Development"),
    ("lutris", "net.lutris.Lutris", "Games"),
    ("superTuxKart", "net.supertuxkart.SuperTuxKart", "Games"),
    ("stellarium", "org.stellarium.Stellarium", "Education"),
    ("wireshark", "org.wireshark.Wireshark", "Security"),
];

/// Whether Store runs on NixOS, where these sources take pacman's place.
pub fn active() -> bool {
    Path::new("/etc/NIXOS").exists()
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

/// nixpkgs attributes in the user's profile, with the element names `nix profile` uses.
fn profile() -> BTreeMap<String, (String, Installed)> {
    let Ok(text) = command("nix", &["profile", "list", "--json"]) else { return BTreeMap::new() };
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let mut out = BTreeMap::new();
    for (element, e) in v["elements"].as_object().into_iter().flatten() {
        let Some(attr) = e["attrPath"].as_str().and_then(|a| a.strip_prefix(SYSTEM)) else { continue };
        if !e["originalUrl"].as_str().is_some_and(|u| u.contains(NIXPKGS)) {
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
                (id.to_string(), Installed { name: id.into(), version: f.next().unwrap_or("").trim().into(), stamp: String::new() })
            })
        })
        .collect()
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
    let mut apps = Vec::new();
    for &(attr, id, category) in PICKS {
        let mut a = by_id.get(id).cloned().unwrap_or_else(|| App { name: attr.into(), desktop: true, ..Default::default() });
        a.package = attr.into();
        a.source = NIXPKGS.into();
        a.category = category.into();
        a.popular = true;
        apps.push(a);
    }
    for a in &mut flathub {
        a.source = FLATHUB.into();
        a.popular = PICKS.iter().any(|(_, id, _)| *id == a.package);
    }
    apps.extend(flathub);

    let mut installed = BTreeMap::new();
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
        repositories: [FLATHUB, NIXPKGS]
            .map(|name| crate::repositories::Repository { name: name.into(), servers: Vec::new(), enabled: true })
            .into(),
        updates,
        warning,
    })
}

/// nixpkgs packages matching `query` (`nix search`), for anything not in the picks.
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

fn plan(request: &Request) -> Result<Plan> {
    let profile = profile();
    let changes = match request {
        Request::Install { source, packages } => {
            ensure!(matches!(source.as_str(), FLATHUB | NIXPKGS), "Install from Flathub or nixpkgs");
            ensure!(!packages.is_empty() && packages.iter().all(|p| valid_name(p)), "Invalid package selection");
            packages
                .iter()
                .map(|p| Change { name: p.clone(), old: None, new: Some(String::new()), source: source.clone(), bytes: 0 })
                .collect()
        }
        Request::Remove { package } => {
            ensure!(valid_name(package), "Invalid package name");
            let source = if profile.contains_key(package) { NIXPKGS } else { FLATHUB };
            vec![Change { name: package.clone(), old: Some(String::new()), new: None, source: source.into(), bytes: 0 }]
        }
        Request::Update => {
            let mut c: Vec<Change> = profile
                .keys()
                .map(|a| Change { name: a.clone(), old: None, new: None, source: NIXPKGS.into(), bytes: 0 })
                .collect();
            c.extend(
                flatpaks()
                    .into_keys()
                    .map(|id| Change { name: id, old: None, new: None, source: FLATHUB.into(), bytes: 0 }),
            );
            c
        }
        _ => bail!("On NixOS, Store installs from Flathub and nixpkgs only"),
    };
    Ok(Plan { changes, snapshot: String::new() })
}

/// Run one step, passing its progress lines on as status.
fn step(send: &async_channel::Sender<Event>, program: &str, args: &[&str]) -> Result<()> {
    let _ = send.send_blocking(Event::Status { message: format!("{program} {}", args.join(" ")) });
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("{program} is missing"))?;
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
    ensure!(child.wait()?.success(), "{}", if last.is_empty() { format!("{program} failed") } else { last });
    Ok(())
}

fn apply(request: &Request, send: &async_channel::Sender<Event>) -> Result<String> {
    match request {
        Request::Install { source, packages } => {
            for p in packages {
                if source == NIXPKGS {
                    step(send, "nix", &["profile", "add", &format!("{NIXPKGS}#{p}")])?;
                } else {
                    step(send, "flatpak", &["install", "--user", "--noninteractive", "-y", FLATHUB, p])?;
                }
            }
            Ok("Installed".into())
        }
        Request::Remove { package } => {
            match profile().get(package) {
                Some((element, _)) => step(send, "nix", &["profile", "remove", element])?,
                None => step(send, "flatpak", &["uninstall", "--user", "--noninteractive", "-y", package])?,
            }
            Ok("Removed".into())
        }
        Request::Update => {
            if !profile().is_empty() {
                step(send, "nix", &["profile", "upgrade", "--all"])?;
            }
            step(send, "flatpak", &["update", "--user", "--noninteractive", "-y"])?;
            Ok("Everything is up to date".into())
        }
        _ => bail!("On NixOS, Store installs from Flathub and nixpkgs only"),
    }
}

/// The window's transaction: a plan to review, then the user's own nix or flatpak.
pub fn start(request: Request, send: async_channel::Sender<Event>, confirmation: std::sync::mpsc::Receiver<Option<Plan>>) {
    let result = (|| -> Result<Option<String>> {
        let plan = plan(&request)?;
        let _ = send.send_blocking(Event::Plan { plan });
        match confirmation.recv() {
            Ok(Some(_)) => apply(&request, &send).map(Some),
            _ => Ok(None),
        }
    })();
    let _ = send.send_blocking(match result {
        Ok(Some(message)) => Event::Done { message },
        Ok(None) => return,
        Err(e) => Event::Error { message: e.to_string() },
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
}
