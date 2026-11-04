use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

pub const STATE: &str = "/var/lib/nyx-store";
pub const HELPER: &str = "/usr/local/lib/nyx-store/nyx-store-helper";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Repository {
    pub name: String,
    pub servers: Vec<String>,
    pub enabled: bool,
}

pub fn official(name: &str) -> bool {
    matches!(name, "omarchy" | "core" | "extra" | "multilib" | crate::nix::FLATHUB | crate::nix::NIXPKGS)
}

pub fn validate(repo: &Repository) -> Result<()> {
    ensure!(crate::model::valid_name(&repo.name), "Invalid repository name");
    ensure!(!official(&repo.name) && repo.name != "aur" && repo.name != "local", "This repository is managed by Omarchy");
    ensure!(!repo.servers.is_empty() && repo.servers.len() <= 12, "Provide at least one HTTPS mirror");
    for server in &repo.servers {
        ensure!(
            server.starts_with("https://")
                && !server.bytes().any(|c| c.is_ascii_whitespace() || c < 32)
                && !server.contains(['\n', '\r', '#', '[', ']', '@']),
            "Repository mirrors must be HTTPS URLs without credentials"
        );
        let expanded = server.replace("$arch", "x86_64").replace("$repo", &repo.name);
        ensure!(!expanded.contains('$'), "Only $arch and $repo substitutions are supported");
    }
    Ok(())
}

pub fn custom() -> Result<Vec<Repository>> {
    let file = Path::new(STATE).join("repositories.json");
    if !file.exists() {
        return Ok(Vec::new());
    }
    Ok(serde_json::from_slice(&fs::read(file)?)?)
}

pub fn save(repository: Repository) -> Result<()> {
    validate(&repository)?;
    let config = system_config()?;
    ensure!(
        repository.name == "blackarch" || !config.repos.iter().any(|r| r.name == repository.name),
        "This source already exists in pacman.conf; manage it with Omarchy"
    );
    if repository.name == "blackarch" && repository.enabled {
        ensure!(
            Path::new("/usr/share/pacman/keyrings/blackarch.gpg").exists(),
            "Use BlackArch’s Setup button to configure its signing keys automatically."
        );
    }
    let mut repos = custom()?;
    if let Some(old) = repos.iter_mut().find(|r| r.name == repository.name) {
        *old = repository;
    } else {
        repos.push(repository);
    }
    let path = Path::new(STATE).join("repositories.json");
    fs::write(path.with_extension("new"), serde_json::to_vec_pretty(&repos)?)?;
    fs::rename(path.with_extension("new"), path)?;
    Ok(())
}

pub fn system_config() -> Result<alpm_utils::config::Config> {
    let mut conf =
        alpm_utils::config::Config::options().pacman_conf_bin("/usr/bin/pacman-conf").pacman_conf("/etc/pacman.conf").read()?;
    ensure!(
        conf.root_dir == "/" && conf.db_path.trim_end_matches('/') == "/var/lib/pacman",
        "Store requires the standard Omarchy pacman root and database path"
    );
    conf.repos.sort_by_key(|r| match r.name.as_str() {
        "omarchy" => 0,
        "core" => 1,
        "extra" => 2,
        "multilib" => 3,
        _ => 4,
    });
    if !conf.repos.iter().any(|r| r.name == "omarchy") {
        bail!("Omarchy repository is missing from pacman.conf");
    }
    Ok(conf)
}

pub fn blackarch() -> Repository {
    Repository {
        name: "blackarch".into(),
        servers: vec![
            "https://blackarch.org/blackarch/blackarch/os/$arch".into(),
            "https://ftp.halifax.rwth-aachen.de/blackarch/os/$arch".into(),
            "https://mirror.cyberbits.eu/blackarch/os/$arch".into(),
        ],
        enabled: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cannot_shadow_official_repositories_or_disable_signatures() {
        let mut repo = blackarch();
        assert!(validate(&repo).is_ok());
        repo.name = "omarchy".into();
        assert!(validate(&repo).is_err());
        repo.name = "thirdparty".into();
        repo.servers = vec!["https://example.test\nSigLevel = Never".into()];
        assert!(validate(&repo).is_err());
        repo.servers = vec!["file:///tmp/packages".into()];
        assert!(validate(&repo).is_err());
    }
}
