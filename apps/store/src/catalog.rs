use crate::{model::App, packages};
use anyhow::{Result, ensure};
use flate2::read::GzDecoder;
use rusqlite::{Connection, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::Read,
    path::Path,
    process::Command,
};

pub const CATEGORIES: &[(&str, &str)] = &[
    ("Internet", "network-workgroup-symbolic"),
    ("Productivity", "accessories-text-editor-symbolic"),
    ("Graphics", "applications-graphics-symbolic"),
    ("Audio & video", "applications-multimedia-symbolic"),
    ("Development", "utilities-terminal-symbolic"),
    ("Games", "applications-games-symbolic"),
    ("Education", "accessories-dictionary-symbolic"),
    ("Security", "security-high-symbolic"),
];

// Editorial selection, not a fabricated popularity chart. Availability comes from pacman's databases.
const PICKS: &[(&str, &str, &str, &str, &str)] = &[
    ("firefox", "Firefox", "A private, customizable web browser", "Internet", "firefox"),
    ("chromium", "Chromium", "A fast, open-source web browser", "Internet", "chromium"),
    ("thunderbird", "Thunderbird", "Email, calendars and contacts together", "Internet", "thunderbird"),
    ("signal-desktop", "Signal", "Private messages and calls", "Internet", "signal-desktop"),
    ("telegram-desktop", "Telegram", "Messaging across all your devices", "Internet", "telegram"),
    ("transmission-gtk", "Transmission", "A lightweight BitTorrent client", "Internet", "transmission"),
    ("libreoffice-fresh", "LibreOffice", "Documents, spreadsheets and presentations", "Productivity", "libreoffice-startcenter"),
    ("obsidian", "Obsidian", "A connected notebook for your ideas", "Productivity", "obsidian"),
    ("keepassxc", "KeePassXC", "Keep passwords in your own encrypted vault", "Productivity", "keepassxc"),
    ("calibre", "Calibre", "Organize and read your ebook library", "Productivity", "calibre-gui"),
    ("gimp", "GIMP", "Edit photos and create digital artwork", "Graphics", "gimp"),
    ("inkscape", "Inkscape", "Draw illustrations, diagrams and vector graphics", "Graphics", "inkscape"),
    ("krita", "Krita", "Paint and illustrate with expressive brushes", "Graphics", "krita"),
    ("blender", "Blender", "Create 3D models, animation and visual effects", "Graphics", "blender"),
    ("darktable", "darktable", "Develop and organize RAW photographs", "Graphics", "darktable"),
    ("vlc", "VLC", "Play almost any video or audio file", "Audio & video", "vlc"),
    ("obs-studio", "OBS Studio", "Record your screen and broadcast live", "Audio & video", "com.obsproject.Studio"),
    ("audacity", "Audacity", "Record, edit and mix audio", "Audio & video", "audacity"),
    ("kdenlive", "Kdenlive", "Edit videos with a full creative timeline", "Audio & video", "kdenlive"),
    ("code", "Code", "A flexible code editor with extensions", "Development", "code"),
    ("zed", "Zed", "A fast editor for writing code together", "Development", "zed"),
    ("godot", "Godot", "Build 2D and 3D games", "Development", "godot"),
    ("steam", "Steam", "Discover, install and play PC games", "Games", "steam"),
    ("lutris", "Lutris", "Bring your game libraries together", "Games", "lutris"),
    ("supertuxkart", "SuperTuxKart", "Race through colorful tracks with friends", "Games", "supertuxkart"),
    ("stellarium", "Stellarium", "Explore the night sky from your desktop", "Education", "stellarium"),
    ("wireshark-qt", "Wireshark", "Inspect and understand network traffic", "Security", "org.wireshark.Wireshark"),
];

fn category(raw: &[String]) -> String {
    for (key, label) in [
        ("Game", "Games"),
        ("Development", "Development"),
        ("Graphics", "Graphics"),
        ("AudioVideo", "Audio & video"),
        ("Audio", "Audio & video"),
        ("Video", "Audio & video"),
        ("Network", "Internet"),
        ("Education", "Education"),
        ("Security", "Security"),
    ] {
        if raw.iter().any(|s| s == key) {
            return label.into();
        }
    }
    "Productivity".into()
}

pub fn parse(xml: &str) -> Result<Vec<App>> {
    let doc = roxmltree::Document::parse(xml)?;
    let mut apps = Vec::new();
    for c in doc.descendants().filter(|n| n.has_tag_name("component") && n.attribute("type") == Some("desktop-application")) {
        let text = |tag| {
            c.children()
                .find(|n| n.has_tag_name(tag) && n.attribute(("http://www.w3.org/XML/1998/namespace", "lang")).is_none())
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
                .to_string()
        };
        // Distribution catalogs name the package; Flathub's names the app (its flatpak ref).
        let package = Some(text("pkgname")).filter(|p| !p.is_empty()).unwrap_or_else(|| {
            c.children()
                .find(|n| n.has_tag_name("bundle") && n.attribute("type") == Some("flatpak"))
                .and_then(|n| n.text())
                .and_then(|r| r.trim().strip_prefix("app/"))
                .and_then(|r| r.split('/').next())
                .unwrap_or("")
                .to_string()
        });
        if !crate::model::valid_name(&package) {
            continue;
        }
        let cats: Vec<_> =
            c.descendants().filter(|n| n.has_tag_name("category")).filter_map(|n| n.text().map(str::to_string)).collect();
        if cats.iter().any(|s| s == "Settings" || s == "System")
            && !cats.iter().any(|s| s == "FileManager" || s == "TerminalEmulator")
        {
            continue;
        }
        let description = c
            .children()
            .find(|n| n.has_tag_name("description") && n.attribute(("http://www.w3.org/XML/1998/namespace", "lang")).is_none())
            .map(|n| {
                n.children()
                    .filter(|n| n.is_element())
                    .map(|n| n.descendants().filter(|n| n.is_text()).filter_map(|n| n.text()).collect::<Vec<_>>().join(" "))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default();
        let screenshots = c
            .descendants()
            .filter(|n| n.has_tag_name("image") && n.attribute("type") == Some("source"))
            .filter_map(|n| n.text())
            .filter(|s| s.starts_with("https://"))
            .map(str::to_string)
            .take(6)
            .collect();
        let cached = c
            .children()
            .filter(|n| n.has_tag_name("icon") && n.attribute("type") == Some("cached"))
            .find(|n| n.attribute("width") == Some("128"))
            .or_else(|| c.children().find(|n| n.has_tag_name("icon") && n.attribute("type") == Some("cached")));
        let icon = if let Some(node) = cached {
            let icon = node.text().unwrap_or("");
            let origin = c.parent().and_then(|n| n.attribute("origin")).unwrap_or("");
            let size = node.attribute("width").filter(|w| ["48", "64", "128"].contains(w)).unwrap_or("64");
            if !icon.contains('/') && !origin.contains('/') && !icon.contains("..") && !origin.contains("..") {
                format!("@cache:{origin}/{size}x{size}/{icon}")
            } else {
                package.clone()
            }
        } else {
            c.children()
                .find(|n| n.has_tag_name("icon") && n.attribute("type") == Some("stock"))
                .and_then(|n| n.text())
                .unwrap_or(&package)
                .to_string()
        };
        let website = c
            .children()
            .find(|n| n.has_tag_name("url") && n.attribute("type") == Some("homepage"))
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();
        let desktop_id = c
            .children()
            .find(|n| n.has_tag_name("launchable") && n.attribute("type") == Some("desktop-id"))
            .and_then(|n| n.text())
            .unwrap_or("")
            .to_string();
        apps.push(App {
            package,
            name: text("name"),
            summary: text("summary"),
            description,
            category: category(&cats),
            icon,
            website,
            license: text("project_license"),
            screenshots,
            desktop_id,
            desktop: true,
            ..Default::default()
        });
    }
    Ok(apps)
}

fn metadata(cache: &Path, refresh: bool, h: &alpm::Alpm) -> Result<Vec<App>> {
    let mut db = Connection::open(cache.join("catalog.sqlite"))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL")?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS metadata (package TEXT PRIMARY KEY, value TEXT NOT NULL)")?;
    if refresh
        || db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))? < 2
        || db.query_row("SELECT count(*) FROM metadata", [], |r| r.get::<_, i64>(0))? == 0
    {
        let mut xmls = Vec::new();
        for dir in ["/usr/share/swcatalog/xml", "/var/cache/swcatalog/xml", "/usr/share/app-info/xmls"] {
            if let Ok(entries) = fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|s| s == "gz" || s == "xml") {
                        let bytes = fs::read(&path)?;
                        let mut xml = String::new();
                        if path.extension().is_some_and(|s| s == "gz") {
                            GzDecoder::new(&bytes[..]).take(128 * 1024 * 1024).read_to_string(&mut xml)?;
                        } else {
                            xml = String::from_utf8(bytes)?;
                        }
                        xmls.push(xml);
                    }
                }
            }
        }
        let archive = cache.join("appstream.pkg.tar.zst");
        if (refresh || !archive.exists())
            && refresh
                && let Some(p) = packages::preferred(h, "archlinux-appstream-data")
                    && let (Some(filename), Some(source)) = (p.filename(), p.db()) {
                        fs::write(&archive, download_metadata(p, source, filename)?)?;
                    }
        if archive.exists() {
            let list = Command::new("/usr/bin/bsdtar").arg("-tf").arg(&archive).output()?;
            ensure!(list.status.success(), "Cannot read Arch app metadata");
            let icons = cache.join("swicons");
            fs::create_dir_all(&icons)?;
            let extracted = Command::new("/usr/bin/bsdtar")
                .arg("-xf")
                .arg(&archive)
                .arg("-C")
                .arg(&icons)
                .args([
                    "--strip-components=4",
                    "--no-same-owner",
                    "--no-same-permissions",
                    "--include",
                    "usr/share/swcatalog/icons/*/64x64/*",
                    "--include",
                    "usr/share/swcatalog/icons/*/128x128/*",
                ])
                .output()?;
            ensure!(extracted.status.success(), "Cannot unpack Arch app icons");
            for name in String::from_utf8_lossy(&list.stdout)
                .lines()
                .filter(|s| s.contains("/xml/") && (s.ends_with(".xml.gz") || s.ends_with(".xml")))
            {
                let data = Command::new("/usr/bin/bsdtar").arg("-xOf").arg(&archive).arg(name).output()?;
                ensure!(data.status.success(), "Cannot read app descriptions");
                let mut xml = String::new();
                if name.ends_with(".gz") {
                    GzDecoder::new(&data.stdout[..]).take(128 * 1024 * 1024).read_to_string(&mut xml)?;
                } else {
                    xml = String::from_utf8(data.stdout)?;
                }
                xmls.push(xml);
            }
        }
        let tx = db.transaction()?;
        for xml in xmls {
            if let Ok(apps) = parse(&xml) {
                for app in apps {
                    tx.execute(
                        "INSERT OR REPLACE INTO metadata VALUES (?1,?2)",
                        params![app.package, serde_json::to_string(&app)?],
                    )?;
                }
            }
        }
        tx.commit()?;
        db.pragma_update(None, "user_version", 2)?;
    }
    let mut stmt = db.prepare("SELECT value FROM metadata")?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows.filter_map(|r| r.ok().and_then(|s| serde_json::from_str(&s).ok())).collect())
}

pub struct Catalog {
    pub apps: Vec<App>,
    pub installed: BTreeMap<String, crate::model::Installed>,
    pub sources: BTreeMap<String, (String, String)>,
    pub candidates: BTreeMap<String, Vec<String>>,
    pub repositories: Vec<crate::repositories::Repository>,
    pub updates: Vec<crate::model::Change>,
    pub warning: String,
}

pub fn load(cache: &Path, refresh: bool) -> Result<Catalog> {
    if crate::nix::active() {
        return crate::nix::load(cache, refresh);
    }
    fs::create_dir_all(cache)?;
    let h = packages::handle(cache, false, refresh)?;
    let installed = packages::installed(&h);
    let sources = crate::provenance::Provenance::sources(&packages::provenance_path())?;
    let (data, warning) = match metadata(cache, refresh, &h) {
        Ok(a) => (a, String::new()),
        Err(e) => (metadata(cache, false, &h).unwrap_or_default(), format!("App descriptions could not refresh: {e}")),
    };
    let mut meta: BTreeMap<String, App> = data.into_iter().map(|a| (a.package.clone(), a)).collect();
    for &(package, name, summary, category, icon) in PICKS {
        let a = meta.entry(package.into()).or_insert_with(|| App {
            package: package.into(),
            name: name.into(),
            summary: summary.into(),
            category: category.into(),
            icon: icon.into(),
            desktop: true,
            ..Default::default()
        });
        if a.icon == package {
            a.icon = icon.into();
        }
        a.popular = true;
        a.category = category.into();
    }
    let mut apps = Vec::new();
    let mut candidates: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut repositories = Vec::new();
    let mut updates = Vec::new();
    for db in h.syncdbs() {
        repositories.push(crate::repositories::Repository {
            name: db.name().into(),
            servers: db.servers().iter().map(str::to_string).collect(),
            enabled: true,
        });
        for p in db.pkgs() {
            candidates.entry(p.name().into()).or_default().push(db.name().into());
            let mut a = meta.get(p.name()).cloned().unwrap_or_else(|| App {
                package: p.name().into(),
                name: p.name().into(),
                summary: p.desc().unwrap_or("").into(),
                ..Default::default()
            });
            a.source = db.name().into();
            a.version = p.version().to_string();
            a.download_size = p.size();
            a.installed = installed.get(p.name()).map(|p| p.version.clone());
            if a.license.is_empty() {
                a.license = p.licenses().iter().collect::<Vec<_>>().join(", ");
            }
            if a.website.is_empty() {
                a.website = p.url().unwrap_or("").into();
            }
            if let Some(old) = installed.get(p.name())
                && alpm::vercmp(p.version().as_str(), old.version.as_str()).is_gt()
                    && sources.get(p.name()).is_some_and(|(s, stamp)| s == db.name() && stamp == &old.stamp)
                {
                    updates.push(crate::model::Change {
                        name: p.name().into(),
                        old: Some(old.version.clone()),
                        new: Some(p.version().to_string()),
                        source: db.name().into(),
                        bytes: p.size(),
                    });
                }
            apps.push(a);
        }
    }
    let names: BTreeSet<_> = apps.iter().map(|a| a.package.clone()).collect();
    for p in h.localdb().pkgs().iter().filter(|p| !names.contains(p.name())) {
        let mut a = meta.get(p.name()).cloned().unwrap_or_else(|| App {
            package: p.name().into(),
            name: p.name().into(),
            summary: p.desc().unwrap_or("").into(),
            ..Default::default()
        });
        a.version = p.version().to_string();
        a.installed = Some(a.version.clone());
        a.source = sources
            .get(p.name())
            .filter(|(_, s)| s == &packages::stamp(p))
            .map(|(s, _)| s.clone())
            .unwrap_or_else(|| "unknown".into());
        enrich_launcher(p, &mut a);
        apps.push(a);
    }
    for a in &mut apps {
        if a.installed.is_some()
            && let Ok(p) = h.localdb().pkg(a.package.as_str()) {
                enrich_launcher(p, a);
            }
    }
    apps.sort_by_key(|a| a.name.to_lowercase());
    for repo in crate::repositories::custom()? {
        if !repo.enabled {
            repositories.push(repo);
        }
    }
    if !repositories.iter().any(|r| r.name == "blackarch") {
        repositories.push(crate::repositories::blackarch());
    }
    Ok(Catalog { apps, installed, sources, candidates, repositories, updates, warning })
}

fn enrich_launcher(p: &alpm::Package, app: &mut App) {
    for file in p.files().files() {
        if !file.name().starts_with(b"usr/share/applications/") || !file.name().ends_with(b".desktop") {
            continue;
        }
        let path = format!("/{}", String::from_utf8_lossy(file.name()));
        let Ok(data) = fs::read_to_string(&path) else {
            continue;
        };
        let mut desktop = BTreeMap::new();
        let mut section = false;
        for line in data.lines() {
            if line.starts_with('[') {
                section = line == "[Desktop Entry]";
                continue;
            }
            if section
                && let Some((key, value)) = line.split_once('=') {
                    desktop.insert(key, value);
                }
        }
        if desktop.get("NoDisplay") == Some(&"true") || desktop.get("Hidden") == Some(&"true") {
            continue;
        }
        let categories = desktop.get("Categories").copied().unwrap_or("");
        if categories.split(';').any(|s| matches!(s, "Settings" | "System"))
            && !categories.split(';').any(|s| matches!(s, "FileManager" | "TerminalEmulator"))
        {
            continue;
        }
        app.desktop = true;
        app.desktop_id = path.rsplit('/').next().unwrap_or("").into();
        if app.name == app.package
            && let Some(name) = desktop.get("Name") {
                app.name = (*name).into();
            }
        if app.icon.is_empty()
            && let Some(icon) = desktop.get("Icon") {
                app.icon = (*icon).into();
            }
        if app.category.is_empty() {
            app.category = category(&categories.split(';').map(str::to_string).collect::<Vec<_>>());
        }
        return;
    }
}

fn download_metadata(package: &alpm::Package, db: &alpm::Db, filename: &str) -> Result<Vec<u8>> {
    use sha2::{Digest, Sha256};
    let expected = package.sha256sum().ok_or_else(|| anyhow::anyhow!("Arch app metadata has no repository checksum"))?;
    let mut urls: Vec<String> =
        db.servers().iter().map(|server| format!("{}/{filename}", server.trim_end_matches('/'))).collect();
    // This is display metadata only. Fetch the identical archive, not a different
    // package version, when an Omarchy snapshot mirror omits the data package.
    urls.push(format!("https://geo.mirror.pkgbuild.com/extra/os/x86_64/{filename}"));
    urls.push(format!("https://archive.archlinux.org/packages/a/archlinux-appstream-data/{filename}"));
    let mut failures = Vec::new();
    for url in urls {
        let result = (|| -> Result<Vec<u8>> {
            let mut response =
                ureq::get(&url).config().timeout_global(Some(std::time::Duration::from_secs(25))).build().call()?;
            let bytes = response.body_mut().with_config().limit(160 * 1024 * 1024).read_to_vec()?;
            ensure!(format!("{:x}", Sha256::digest(&bytes)) == expected, "repository checksum mismatch");
            Ok(bytes)
        })();
        match result {
            Ok(bytes) => return Ok(bytes),
            Err(error) => failures.push(format!("{url}: {error}")),
        }
    }
    anyhow::bail!("Cannot download verified Arch app metadata after trying all mirrors. {}", failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_only_lists_desktop_components() {
        let apps = parse(r#"<components><component type="runtime"><pkgname>libexample</pkgname></component><component type="desktop-application"><pkgname>example</pkgname><name>Example</name><name xml:lang="de">Beispiel</name><summary>Make things</summary><categories><category>Graphics</category></categories><description><p>A &amp; B</p></description><launchable type="desktop-id">example.desktop</launchable></component></components>"#).unwrap();
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "Example");
        assert_eq!(apps[0].category, "Graphics");
        assert_eq!(apps[0].description, "A & B");
    }
}
