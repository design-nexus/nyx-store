//! Arch (Omarchy): the NyxOS software catalog's Nyx apps and presets, from the catalog
//! built into nyx_catalog. Nyx apps install into the home folder with the suite's
//! published installer, as its README gives it (`curl -fsSL …/install.sh | bash -s --
//! APP`), which also updates them; its uninstaller removes the apps it lists. A preset's
//! other items are matched to Arch and AUR packages by name.

use crate::{
    catalog::{Bundle, BundleItem, Group},
    model::{App, Change, Event, Plan, Request},
};
use anyhow::{Context, Result, bail, ensure};
use nyx_catalog::{Catalog as Software, Item};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// Where Nyx apps come from on Arch: the suite's installer.
pub const SUITE: &str = "nyx-suite";
const RAW: &str = "https://raw.githubusercontent.com/design-nexus/nyx/main";

/// The Nyx apps, by name (`media-player`), that the published installer installs and
/// the uninstaller removes.
#[derive(Clone, Debug, Default)]
pub struct Support {
    pub install: BTreeSet<String>,
    pub remove: BTreeSet<String>,
}

/// What Arch gets from the NyxOS catalog.
#[derive(Default)]
pub struct Loaded {
    pub support: Support,
    pub apps: Vec<App>,
    /// The Nyx apps' categories.
    pub groups: Vec<Group>,
    pub bundles: Vec<Bundle>,
    pub warning: String,
}

/// The Nyx apps and the bundles. `repo` finds a repository package by its name or by
/// what it provides, as (repository, package).
pub fn load(cache: &Path, refresh: bool, repo: impl Fn(&str) -> Option<(String, String)>) -> Loaded {
    let sw = match Software::load() {
        Ok(sw) => sw,
        Err(e) => return Loaded { warning: format!("The NyxOS catalog couldn't be read: {e}"), ..Default::default() },
    };
    let mut warnings = Vec::new();
    let support = support(cache, refresh).unwrap_or_else(|e| {
        warnings.push(format!("The Nyx apps' installer couldn't be reached: {e}"));
        Support::default()
    });
    let apps = apps(&sw, &support);
    let groups = crate::nix::groups(&sw).into_iter().filter(|g| g.section.0 == "nyx-apps").collect();
    let wanted: BTreeSet<String> = preset_items(&sw)
        .filter(|i| i.nyx.is_empty())
        .flat_map(|i| {
            let (attrs, whole) = names(i);
            attrs.into_iter().chain(whole)
        })
        .filter(|n| repo(n).is_none())
        .collect();
    let (aur, warning) = aur(cache, refresh, &wanted);
    warnings.extend(warning);
    let bundles = bundles(&sw, &apps, &repo, &aur);
    Loaded { support, apps, groups, bundles, warning: warnings.join(" ") }
}

fn preset_items(sw: &Software) -> impl Iterator<Item = &Item> {
    sw.presets.iter().flat_map(|p| &p.items).filter_map(|id| sw.item(id))
}

fn fetch(url: &str) -> Result<String> {
    let mut response = ureq::get(url).config().timeout_global(Some(std::time::Duration::from_secs(15))).build().call()?;
    Ok(response.body_mut().with_config().limit(4 * 1024 * 1024).read_to_string()?)
}

/// The suite's manifest and uninstaller, fetched when missing or asked, and kept.
fn support(cache: &Path, refresh: bool) -> Result<Support> {
    let manifest = cache.join("nyx-suite.toml");
    let uninstaller = cache.join("nyx-uninstall.sh");
    for (path, file) in [(&manifest, "suite.toml"), (&uninstaller, "uninstall.sh")] {
        if refresh || !path.exists() {
            match fetch(&format!("{RAW}/{file}")) {
                Ok(text) => fs::write(path, text)?,
                Err(e) if !path.exists() => return Err(e),
                Err(_) => {}
            }
        }
    }
    Ok(Support { install: installable(&fs::read_to_string(&manifest)?)?, remove: removable(&fs::read_to_string(&uninstaller)?) })
}

fn installable(manifest: &str) -> Result<BTreeSet<String>> {
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        applications: Vec<Application>,
    }
    #[derive(Deserialize)]
    struct Application {
        id: String,
    }
    let m: Manifest = toml::from_str(manifest).context("The suite's manifest couldn't be read")?;
    Ok(m.applications.into_iter().map(|a| a.id).filter(|a| valid(a)).collect())
}

/// The apps the uninstaller knows: its `ALL=(…)` list.
fn removable(script: &str) -> BTreeSet<String> {
    script
        .lines()
        .find_map(|l| l.strip_prefix("ALL=(")?.strip_suffix(')'))
        .map(|l| l.split_whitespace().filter(|a| valid(a)).map(String::from).collect())
        .unwrap_or_default()
}

/// A Nyx app's name as the installer takes it: lowercase words joined by dashes.
fn valid(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 40
        && !name.starts_with('-')
        && name.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

fn bin_dir() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin")
}

/// Whether a Nyx app is installed: its program in `~/.local/bin`, as `nyx list` checks.
pub fn installed(name: &str) -> bool {
    let program = if name == "cloud" { "nyx-cloudd".to_string() } else { format!("nyx-{name}") };
    bin_dir().join(program).is_file()
}

/// Whether a launcher (`io.github.design_nexus.nyx.Mail.desktop`) is installed.
fn launcher(desktop_id: &str) -> bool {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let user = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    std::iter::once(user)
        .chain(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from))
        .any(|d| d.join("applications").join(desktop_id).is_file())
}

/// The catalog's Nyx apps that are installed here or that the installer has. One
/// installed without a launcher (Cloud's service) can't be opened.
fn apps(sw: &Software, support: &Support) -> Vec<App> {
    let groups = crate::nix::groups(sw);
    sw.items
        .iter()
        .filter_map(|item| {
            let name = item.nyx.first()?;
            let installed = installed(name);
            if !installed && !support.install.contains(name) {
                return None;
            }
            let group = groups.iter().find(|g| g.id == item.category).cloned().unwrap_or_default();
            let id = crate::nix::nyx_app_id(name);
            let desktop_id = format!("{id}.desktop");
            Some(App {
                package: item.id.clone(),
                name: item.name.clone(),
                summary: item.summary.clone(),
                description: description(item, name, support),
                category: crate::nix::category(&item.category).into(),
                icon: id,
                desktop_id: if installed && !launcher(&desktop_id) { String::new() } else { desktop_id },
                source: SUITE.into(),
                desktop: true,
                nyx: true,
                scope: "you".into(),
                keywords: item.names(),
                group: (group.id.clone(), group.name.clone()),
                section: group.section.clone(),
                installed: installed.then(String::new),
                ..Default::default()
            })
        })
        .collect()
}

fn description(item: &Item, name: &str, support: &Support) -> String {
    let mut lines = Vec::new();
    if support.install.contains(name) {
        lines.push("A Nyx app, installed for you into your home folder (~/.local) from its published release by the Nyx installer, which updates it the same way.".to_string());
        if !support.remove.contains(name) {
            lines.push("The Nyx uninstaller doesn't list it yet, so Store can't remove it.".into());
        }
    } else {
        lines.push("A Nyx app. It has no published release yet, so Store can open it but not update or remove it.".into());
    }
    if let Some(note) = &item.note {
        lines.push(note.clone());
    }
    format!("{}\n\n{}", item.summary, lines.join("\n"))
}

/// The names an item's Arch package might have: its nixpkgs attributes' own names
/// (`kdePackages.kdenlive` is `kdenlive`) and, for an item that's one program, its id
/// and aliases too (`zed` for `zed-editor`). Never its command, which other packages
/// share: the AUR's `omp` is a music player, not oh-my-pi.
fn names(item: &Item) -> (Vec<String>, Vec<String>) {
    let attrs: Vec<String> = item.packages.iter().map(|p| p.rsplit('.').next().unwrap_or(p).to_string()).collect();
    let whole = if attrs.len() <= 1 {
        std::iter::once(&item.id).chain(&item.aliases).filter(|n| !attrs.contains(n)).cloned().collect()
    } else {
        Vec::new()
    };
    (attrs, whole)
}

/// The names the AUR has, of `wanted`: asked once and kept until a refresh, or until the
/// names change.
fn aur(cache: &Path, refresh: bool, wanted: &BTreeSet<String>) -> (BTreeSet<String>, Option<String>) {
    #[derive(Serialize, Deserialize)]
    struct Kept {
        wanted: BTreeSet<String>,
        found: BTreeSet<String>,
    }
    let path = cache.join("nyx-bundles-aur.json");
    let kept: Option<Kept> = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str(&t).ok());
    if let Some(k) = &kept
        && !refresh
        && &k.wanted == wanted
    {
        return (k.found.clone(), None);
    }
    if wanted.is_empty() {
        return (BTreeSet::new(), None);
    }
    match crate::aur::info(&wanted.iter().cloned().collect::<Vec<_>>()) {
        Ok(found) => {
            let found: BTreeSet<String> = found.into_iter().map(|a| a.package).filter(|p| wanted.contains(p)).collect();
            if let Ok(text) = serde_json::to_string(&Kept { wanted: wanted.clone(), found: found.clone() }) {
                let _ = fs::write(&path, text);
            }
            (found, None)
        }
        Err(e) => match kept {
            Some(k) => (k.found, None),
            None => (BTreeSet::new(), Some(format!("The AUR couldn't be checked for the bundles' packages: {e}"))),
        },
    }
}

/// The catalog's presets with what each item installs here.
fn bundles(sw: &Software, nyx: &[App], repo: &impl Fn(&str) -> Option<(String, String)>, aur: &BTreeSet<String>) -> Vec<Bundle> {
    sw.presets
        .iter()
        .map(|p| {
            let mut b = Bundle {
                id: p.id.clone(),
                name: p.name.clone(),
                summary: p.summary.clone(),
                items: p.items.clone(),
                ..Default::default()
            };
            for item in p.items.iter().filter_map(|id| sw.item(id)) {
                let here = arch_item(item, nyx, repo, aur);
                if here.packages.is_empty() {
                    b.left_out.push(item.name.clone());
                } else {
                    b.arch.push(here);
                }
            }
            b
        })
        .collect()
}

/// An item as it installs here: a Nyx app from the installer; otherwise each nixpkgs
/// attribute's package from the repositories or else the AUR, and the packages of the
/// services it turns on (`docker`), by exact name.
fn arch_item(item: &Item, nyx: &[App], repo: &impl Fn(&str) -> Option<(String, String)>, aur: &BTreeSet<String>) -> BundleItem {
    let mut out =
        BundleItem { id: item.id.clone(), name: item.name.clone(), services: item.services.clone(), ..Default::default() };
    if !item.nyx.is_empty() {
        if nyx.iter().any(|a| a.package == item.id) {
            out.packages.push((SUITE.into(), item.id.clone()));
        }
        return out;
    }
    let in_aur = |n: &String| aur.contains(n).then(|| ("aur".to_string(), n.clone()));
    let (attrs, whole) = names(item);
    if attrs.len() <= 1 {
        let all: Vec<&String> = attrs.iter().chain(&whole).collect();
        match all.iter().find_map(|n| repo(n)).or_else(|| all.iter().find_map(|n| in_aur(n))) {
            Some(p) => out.packages.push(p),
            None => out.missing = attrs,
        }
    } else {
        for attr in attrs {
            match repo(&attr).or_else(|| in_aur(&attr)) {
                Some(p) if !out.packages.contains(&p) => out.packages.push(p),
                Some(_) => {}
                None => out.missing.push(attr),
            }
        }
    }
    if !out.packages.is_empty() {
        for service in &item.services {
            if let Some(p) = repo(service).filter(|(_, name)| name == service)
                && !out.packages.contains(&p)
            {
                out.packages.push(p);
            }
        }
    }
    out
}

/// The changes to review: each app, installed or not, and what it becomes.
fn plan(action: &str, apps: &[String]) -> Result<Plan> {
    ensure!(matches!(action, "install" | "update" | "remove"), "Unknown Nyx app action: {action}");
    ensure!(!apps.is_empty() && apps.len() <= 64 && apps.iter().all(|a| valid(a)), "Invalid Nyx app selection");
    ensure!(action != "remove" || !apps.iter().any(|a| a == "store"), "Store can't remove itself");
    let sw = Software::load()?;
    let changes = apps
        .iter()
        .map(|a| Change {
            name: sw
                .items
                .iter()
                .find(|i| i.nyx.first() == Some(a))
                .map(|i| i.name.clone())
                .unwrap_or_else(|| format!("nyx-{a}")),
            old: installed(a).then(|| "Installed".into()),
            new: (action != "remove").then(|| "Latest release".into()),
            source: SUITE.into(),
            bytes: 0,
        })
        .collect();
    Ok(Plan { changes, ..Default::default() })
}

/// The window's Nyx app request: the review, then the installer or uninstaller.
pub fn start(request: Request, send: async_channel::Sender<Event>, confirmation: std::sync::mpsc::Receiver<Option<Plan>>) {
    let result = (|| -> Result<Option<String>> {
        let Request::NyxApps { change: action, apps } = &request else { bail!("This isn't a Nyx app request") };
        let plan = plan(action, apps)?;
        let _ = send.send_blocking(Event::Plan { plan: plan.clone() });
        match confirmation.recv() {
            Ok(Some(approved)) => {
                ensure!(approved == plan, "The approved changes differ from the reviewed ones");
                run(action, apps, &send).map(Some)
            }
            _ => Ok(None),
        }
    })();
    let _ = send.send_blocking(match result {
        Ok(Some(message)) => Event::Done { message },
        Ok(None) => return,
        Err(e) => Event::Error { message: e.to_string() },
    });
}

/// The command the review shows and `run` runs.
pub fn command(action: &str, apps: &[String]) -> String {
    let script = if action == "remove" { "uninstall.sh" } else { "install.sh" };
    format!("curl -fsSL {RAW}/{script} | bash -s -- {}", apps.join(" "))
}

/// The suite's script from GitHub, piped to bash with the apps as its arguments, its
/// output passed on as status.
fn run(action: &str, apps: &[String], send: &async_channel::Sender<Event>) -> Result<String> {
    let script = if action == "remove" { "uninstall.sh" } else { "install.sh" };
    let _ = send.send_blocking(Event::Status { message: command(action, apps) });
    let mut child = Command::new("/usr/bin/bash")
        .args(["-c", r#"set -o pipefail; curl -fsSL "$0" | bash -s -- "$@""#, &format!("{RAW}/{script}")])
        .args(apps)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("bash is missing")?;
    let stdout = child.stdout.take().unwrap();
    let out_send = send.clone();
    let out = std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let line = plain(&line);
            if !line.trim().is_empty() {
                let _ = out_send.send_blocking(Event::Status { message: line });
            }
        }
    });
    let mut last = String::new();
    for line in BufReader::new(child.stderr.take().unwrap()).lines().map_while(Result::ok) {
        let line = plain(&line);
        if !line.trim().is_empty() {
            let _ = send.send_blocking(Event::Status { message: line.clone() });
            last = line;
        }
    }
    let _ = out.join();
    ensure!(
        child.wait()?.success(),
        "{}",
        if last.is_empty() {
            "The Nyx installer stopped before it finished".to_string()
        } else {
            last.trim_start_matches("==> ").to_string()
        }
    );
    Ok(match action {
        "remove" => "Removed".into(),
        "update" => "Updated. Reopen the apps to use the new versions.".into(),
        _ => "Installed".into(),
    })
}

/// A line without its terminal colours.
fn plain(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builtin() -> Software {
        Software::builtin().unwrap()
    }

    #[test]
    fn nyx_items_are_named_for_their_app() {
        for item in builtin().items.iter().filter(|i| !i.nyx.is_empty()) {
            let app = App { package: item.id.clone(), source: SUITE.into(), ..Default::default() };
            assert_eq!(app.suite_name(), Some(item.nyx[0].as_str()), "{}", item.id);
        }
    }

    #[test]
    fn reads_what_the_installer_and_uninstaller_support() {
        let manifest = "schema_version = 1\n[[applications]]\nid = \"mail\"\nrepository = \"mail\"\n[[applications]]\nid = \"media-player\"\n";
        assert_eq!(installable(manifest).unwrap(), BTreeSet::from(["mail".to_string(), "media-player".to_string()]));
        let script = "#!/usr/bin/env bash\nset -euo pipefail\nALL=(settings music media-player)\nsay() { :; }\n";
        assert_eq!(removable(script), BTreeSet::from(["settings", "music", "media-player"].map(String::from)));
        assert!(removable("echo nothing").is_empty());
        assert!(!valid("--purge") && !valid("a b") && !valid("Mail") && valid("control-center"));
    }

    #[test]
    fn names_come_from_attributes_ids_and_aliases_not_commands() {
        let sw = builtin();
        assert_eq!(names(sw.item("zed").unwrap()), (vec!["zed-editor".to_string()], vec!["zed".to_string()]));
        assert_eq!(names(sw.item("oh-my-pi").unwrap()), (vec![], vec!["oh-my-pi".to_string()]));
        assert!(names(sw.item("c-cpp").unwrap()).1.is_empty());
    }

    #[test]
    fn bundles_match_what_arch_has_and_say_what_they_leave_out() {
        let sw = builtin();
        let repo = |name: &str| {
            let found = match name {
                "zed" | "neovim" | "cmake" | "gcc" | "podman" | "podman-compose" | "docker" | "docker-compose" => name,
                "python3" => "python",
                "pkg-config" => "pkgconf",
                _ => return None,
            };
            Some(("extra".to_string(), found.to_string()))
        };
        let aur = BTreeSet::from(["vscodium".to_string(), "oh-my-pi".to_string()]);
        let nyx = vec![App { package: "nyx-git".into(), source: SUITE.into(), ..Default::default() }];
        let bundles = bundles(&sw, &nyx, &repo, &aur);
        let developer = bundles.iter().find(|b| b.id == "developer").unwrap();
        let item = |id: &str| developer.arch.iter().find(|i| i.id == id);
        assert_eq!(item("zed").unwrap().packages, [("extra".to_string(), "zed".to_string())]);
        assert_eq!(item("vscodium").unwrap().packages, [("aur".to_string(), "vscodium".to_string())]);
        assert_eq!(item("nyx-git").unwrap().packages, [(SUITE.to_string(), "nyx-git".to_string())]);
        let c = item("c-cpp").unwrap();
        assert_eq!(c.packages.len(), 3);
        assert_eq!(c.missing, ["clang-tools", "ninja", "gnumake"]);
        assert_eq!(item("podman").unwrap().packages.len(), 2);
        assert!(developer.left_out.contains(&"GitHub CLI".to_string()));
        assert!(developer.left_out.contains(&"Nyx Containers".to_string()));
        let homelab = bundles.iter().find(|b| b.id == "homelab").unwrap();
        let docker = homelab.arch.iter().find(|i| i.id == "docker").unwrap();
        assert_eq!(docker.packages.iter().map(|p| p.1.as_str()).collect::<Vec<_>>(), ["docker-compose", "docker"]);
        let ai = bundles.iter().find(|b| b.id == "ai-developer").unwrap();
        assert!(ai.arch.iter().any(|i| i.id == "oh-my-pi" && i.packages[0].0 == "aur"));
    }

    #[test]
    fn reviews_each_app() {
        let mail = plan("install", &["mail".to_string()]).unwrap();
        assert_eq!(mail.changes.len(), 1);
        assert_eq!(mail.changes[0].name, "Nyx Mail");
        assert_eq!(mail.changes[0].new.as_deref(), Some("Latest release"));
        assert!(plan("remove", &["store".to_string()]).is_err());
        assert!(plan("install", &["--help".to_string()]).is_err());
        assert!(plan("purge", &["mail".to_string()]).is_err());
        assert_eq!(command("remove", &["mail".to_string()]), format!("curl -fsSL {RAW}/uninstall.sh | bash -s -- mail"));
    }

    #[test]
    fn strips_terminal_colours() {
        assert_eq!(plain("\u{1b}[1m==>\u{1b}[0m Installing mail"), "==> Installing mail");
    }
}
