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
