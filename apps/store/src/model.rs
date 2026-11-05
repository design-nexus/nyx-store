use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct App {
    pub package: String,
    pub name: String,
    pub summary: String,
    pub description: String,
    pub category: String,
    pub icon: String,
    pub screenshots: Vec<String>,
    pub website: String,
    pub license: String,
    pub source: String,
    pub version: String,
    pub installed: Option<String>,
    pub desktop_id: String,
    pub desktop: bool,
    pub popular: bool,
    pub download_size: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Installed {
    pub name: String,
    pub version: String,
    pub stamp: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Change {
    pub name: String,
    pub old: Option<String>,
    pub new: Option<String>,
    pub source: String,
    pub bytes: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub snapshot: String,
    /// What the person agrees to before applying (unfree licenses, services, groups).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Request {
    Install { source: String, packages: Vec<String> },
    Remove { package: String },
    Update,
    /// NixOS: back to the system generation before this one.
    Rollback,
    BlackarchSetup,
    Sources { assignments: Vec<(String, String, String)> },
    Repository { repository: crate::repositories::Repository },
    TrustKey { fingerprint: String },
    AurInstall { artifacts: Vec<String>, base: String, commit: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "kebab-case")]
pub enum Event {
    Status { message: String },
    Plan { plan: Plan },
    Done { message: String },
    Error { message: String },
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 180
        && !name.starts_with(['-', '.'])
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"@._+-".contains(&c))
}

pub fn protected(name: &str) -> bool {
    name.starts_with("omarchy")
        || name.starts_with("hypr")
        || name.starts_with("linux")
        || name.starts_with("systemd")
        || matches!(
            name,
            "pacman"
                | "glibc"
                | "filesystem"
                | "base"
                | "bash"
                | "sudo"
                | "polkit"
                | "dbus"
                | "mkinitcpio"
                | "limine"
                | "grub"
                | "mesa"
                | "nyx-store"
        )
}

/// The installed app a launcher (`firefox` or `firefox.desktop`) belongs to: by its
/// desktop id, by package (Flatpak ids are both), or by `owner`, the package that owns
/// the launcher's file.
pub fn installed_app<'a>(apps: &'a [App], desktop_id: &str, owner: Option<&str>) -> Option<&'a App> {
    let stem = desktop_id.trim_end_matches(".desktop");
    let installed = || apps.iter().filter(|a| a.installed.is_some());
    installed()
        .find(|a| !a.desktop_id.is_empty() && a.desktop_id.trim_end_matches(".desktop") == stem)
        .or_else(|| installed().find(|a| a.package == stem))
        .or_else(|| owner.and_then(|o| installed().find(|a| a.package == o)))
}

/// The package name in a Nix store path (`/nix/store/HASH-firefox-128.0/…` is
/// `firefox`): the name before the first part that starts with a digit.
pub fn nix_store_name(path: &str) -> Option<String> {
    let entry = path.strip_prefix("/nix/store/")?.split('/').next()?;
    let (_, name) = entry.split_once('-')?;
    let parts: Vec<&str> = name.split('-').take_while(|p| !p.starts_with(|c: char| c.is_ascii_digit())).collect();
    (!parts.is_empty()).then(|| parts.join("-"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(package: &str, desktop_id: &str, installed: bool) -> App {
        App {
            package: package.into(),
            desktop_id: desktop_id.into(),
            installed: installed.then(|| "1".into()),
            ..Default::default()
        }
    }

    #[test]
    fn finds_the_app_a_launcher_belongs_to() {
        let apps = [
            app("gimp", "gimp.desktop", false),
            app("gimp", "gimp.desktop", true),
            app("org.gnome.Maps", "", true),
            app("signal-desktop", "", true),
        ];
        assert_eq!(installed_app(&apps, "gimp", None).map(|a| a.installed.is_some()), Some(true));
        assert_eq!(installed_app(&apps, "org.gnome.Maps.desktop", None).map(|a| a.package.as_str()), Some("org.gnome.Maps"));
        assert_eq!(installed_app(&apps, "signal", Some("signal-desktop")).map(|a| a.package.as_str()), Some("signal-desktop"));
        assert!(installed_app(&apps, "webapp-hey", None).is_none());
    }

    #[test]
    fn nix_store_names() {
        assert_eq!(
            nix_store_name("/nix/store/abc123-firefox-128.0/share/applications/firefox.desktop").as_deref(),
            Some("firefox")
        );
        assert_eq!(nix_store_name("/nix/store/abc-signal-desktop-7.1.0/x").as_deref(), Some("signal-desktop"));
        assert_eq!(nix_store_name("/usr/share/applications/x.desktop"), None);
    }
}
