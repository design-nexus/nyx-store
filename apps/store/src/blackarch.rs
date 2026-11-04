//! A narrowly scoped bootstrap: verify the keyring, install only that package,
//! populate its trust, and enable a Store-only source. Never run strap.sh.
use crate::{
    model::{Change, Event, Plan},
    packages,
    provenance::Provenance,
    repositories,
    transaction::emit,
};
use alpm::{Alpm, SigLevel};
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

// Bootstrap signers published by https://blackarch.org/strap.sh.
const SIGNERS: &[&str] = &[
    "8F9A9793CB8591147C2EC70566E0CDBD1E01F333",
    "A0917C4147A37007CB54C1CFD295AA940EFDDF62",
    "4345771566D76038C7FEB43863EC0ADBEA87E4E3",
    "F9A6E68A711354D84A9B91637533BAFE69A25079",
];
pub struct Verified {
    pub package: PathBuf,
    pub fingerprint: String,
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    Ok(ureq::get(url)
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .call()?
        .body_mut()
        .with_config()
        .limit(limit)
        .read_to_vec()?)
}
fn field<'a>(desc: &'a str, name: &str) -> Option<&'a str> {
    desc.lines().skip_while(|s| *s != name).nth(1).map(str::trim)
}
fn signer(status: &str) -> Option<String> {
    if status.lines().any(|line| {
        line.split_whitespace().nth(1).is_some_and(|s| ["BADSIG", "ERRSIG", "REVKEYSIG", "EXPKEYSIG", "EXPSIG"].contains(&s))
    }) {
        return None;
    }
    status
        .lines()
        .filter_map(|line| {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.get(1) != Some(&"VALIDSIG") {
                return None;
            }
            let primary = parts.last().filter(|s| s.len() == 40).or_else(|| parts.get(2))?;
            SIGNERS.contains(primary).then(|| primary.to_string())
        })
        .next()
}
struct Agent(PathBuf);
impl Drop for Agent {
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/gpgconf").arg("--homedir").arg(&self.0).args(["--kill", "all"]).output();
    }
}

pub fn verify_bootstrap(directory: &Path, progress: impl Fn(&str)) -> Result<Verified> {
    fs::create_dir_all(directory)?;
    fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
    progress("Downloading BlackArch repository support…");
    let mut selected = None;
    let mut errors = Vec::new();
    for server in repositories::blackarch().servers {
        let server = server.replace("$arch", "x86_64");
        let attempt = (|| -> Result<PathBuf> {
            let database = directory.join("blackarch.db");
            fs::write(&database, download(&format!("{server}/blackarch.db"), 32 * 1024 * 1024)?)?;
            let output = Command::new("/usr/bin/bsdtar").arg("-xOf").arg(&database).arg("blackarch-keyring-*/desc").output()?;
            ensure!(output.status.success(), "Cannot find the BlackArch keyring");
            let desc = String::from_utf8(output.stdout)?;
            let filename = field(&desc, "%FILENAME%").context("Keyring package name is missing")?;
            ensure!(
                filename.starts_with("blackarch-keyring-")
                    && filename.contains(".pkg.tar.")
                    && filename.bytes().all(|c| c.is_ascii_alphanumeric() || b"._+-".contains(&c)),
                "Invalid keyring package filename"
            );
            let package = directory.join(filename);
            fs::write(&package, download(&format!("{server}/{filename}"), 16 * 1024 * 1024)?)?;
            fs::write(package.with_extension("sig"), download(&format!("{server}/{filename}.sig"), 128 * 1024)?)?;
            Ok(package)
        })();
        match attempt {
            Ok(path) => {
                selected = Some(path);
                break;
            }
            Err(error) => errors.push(error.to_string()),
        }
    }
    let package = selected.ok_or_else(|| anyhow::anyhow!("Cannot download BlackArch support: {}", errors.join("; ")))?;
    progress("Verifying BlackArch signing keys…");
    let keys = directory.join("gnupg");
    fs::create_dir_all(&keys)?;
    fs::set_permissions(&keys, fs::Permissions::from_mode(0o700))?;
    let _agent = Agent(keys.clone());
    let mut imported = 0;
    for fingerprint in SIGNERS {
        if let Ok(key) =
            download(&format!("https://keyserver.ubuntu.com/pks/lookup?op=get&search=0x{fingerprint}"), 2 * 1024 * 1024)
        {
            let file = directory.join(format!("{fingerprint}.asc"));
            fs::write(&file, key)?;
            let output = Command::new("/usr/bin/gpg")
                .arg("--homedir")
                .arg(&keys)
                .args(["--batch", "--no-tty", "--import"])
                .arg(file)
                .output()?;
            if output.status.success() {
                imported += 1;
            }
        }
    }
    ensure!(imported > 0, "Could not retrieve BlackArch’s published signing keys. Please retry setup.");
    let verified = Command::new("/usr/bin/gpg")
        .arg("--homedir")
        .arg(&keys)
        .args(["--batch", "--no-tty", "--status-fd", "1", "--verify"])
        .arg(package.with_extension("sig"))
        .arg(&package)
        .output()?;
    let fingerprint = signer(&String::from_utf8_lossy(&verified.stdout))
        .context("The keyring signature does not match BlackArch’s published signers")?;
    ensure!(
        verified.status.success(),
        "BlackArch keyring signature verification failed: {}",
        String::from_utf8_lossy(&verified.stderr)
    );
    Ok(Verified { package, fingerprint })
}

pub fn setup(handle: &mut Alpm, provenance: &mut Provenance) -> Result<()> {
    let state = Path::new(repositories::STATE);
    emit(Event::Status { message: "Checking installed BlackArch support…".into() });
    let already_installed = handle.localdb().pkg("blackarch-keyring").is_ok();
    if !already_installed || !Path::new("/usr/share/pacman/keyrings/blackarch.gpg").exists() {
        ensure!(
            !packages::preferred(handle, "blackarch-keyring").and_then(|p| p.db()).is_some_and(|d| d.name() == "omarchy"),
            "Omarchy provides this keyring and must retain priority; install its package first"
        );
        let verified =
            verify_bootstrap(&state.join("blackarch-setup"), |message| emit(Event::Status { message: message.into() }))?;
        if let Ok(old) = handle.localdb().pkg("blackarch-keyring") {
            let sources = Provenance::sources(&packages::provenance_path())?;
            ensure!(
                sources.get(old.name()).is_some_and(|(source, stamp)| source == "blackarch" && stamp == &packages::stamp(old)),
                "The installed keyring has an unconfirmed or different source. Review its original source before repairing it."
            );
        }
        emit(Event::Status { message: "Installing BlackArch repository support…".into() });
        handle.set_progress_cb((), |_, package, percent, _, _, _| {
            emit(Event::Status { message: format!("Installing {package} · {percent}%") });
        });
        handle.set_event_cb((), |event, _| {
            if let alpm::Event::ScriptletInfo(info) = event.event() {
                emit(Event::Status { message: info.line().into() });
            }
        });
        let result = (|| -> Result<()> {
            // The detached signature was checked above against pinned primary keys;
            // bootstrap does not weaken pacman's package signature policy.
            let package = handle.pkg_load(verified.package.to_string_lossy().as_bytes(), true, SigLevel::NONE)?;
            ensure!(package.name() == "blackarch-keyring", "Unexpected bootstrap package");
            if let Ok(old) = handle.localdb().pkg("blackarch-keyring") {
                ensure!(package.version() >= old.version(), "Refusing a keyring downgrade");
            }
            handle.trans_add_pkg(package).map_err(|e| anyhow::anyhow!("Cannot install the keyring: {e:?}"))?;
            handle.trans_prepare().map_err(|e| anyhow::anyhow!("Cannot prepare BlackArch support: {e:?}"))?;
            ensure!(
                handle.trans_remove().is_empty()
                    && handle.trans_add().len() == 1
                    && handle.trans_add().iter().all(|p| p.name() == "blackarch-keyring"),
                "BlackArch setup would change unrelated packages"
            );
            let package = handle.trans_add().first().unwrap();
            let plan = Plan {
                snapshot: packages::snapshot(handle),
                changes: vec![Change {
                    name: package.name().into(),
                    old: handle.localdb().pkg(package.name()).ok().map(|p| p.version().to_string()),
                    new: Some(package.version().to_string()),
                    source: "blackarch".into(),
                    bytes: package.size(),
                }],
                notes: Vec::new(),
            };
            provenance.begin(&plan)?;
            handle.trans_commit().map_err(|e| anyhow::anyhow!("Cannot install BlackArch support: {e:?}"))?;
            fs::write(state.join("blackarch-bootstrap-signer"), verified.fingerprint)?;
            Ok(())
        })();
        let _ = handle.trans_release();
        let fresh = packages::handle(state, true, false).context("Reopening the installed package database")?;
        provenance.reconcile(&packages::installed(&fresh))?;
        result?;
    }
    if already_installed {
        emit(Event::Status { message: "Using the installed BlackArch keyring; no download is needed.".into() });
    }
    // Also release the native lock when an earlier attempt installed the keyring.
    let _ = handle.trans_release();
    emit(Event::Status { message: "Configuring BlackArch signing keys…".into() });
    let output = Command::new("/usr/bin/pacman-key").args(["--populate", "blackarch"]).output()?;
    ensure!(output.status.success(), "Cannot configure BlackArch signing keys: {}", String::from_utf8_lossy(&output.stderr));
    emit(Event::Status { message: "Enabling the BlackArch repository…".into() });
    let mut repository = repositories::blackarch();
    repository.enabled = true;
    repositories::save(repository).context("Saving the BlackArch repository")?;
    emit(Event::Done { message: "BlackArch is ready".into() });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_only_published_primary_signers() {
        assert!(signer("[GNUPG:] GOODSIG 1234 someone").is_none());
        assert!(signer("[GNUPG:] VALIDSIG 0000000000000000000000000000000000000000 date 0 0 4 0 1 10 00").is_none());
        assert_eq!(signer(&format!("[GNUPG:] VALIDSIG SUBKEY date 0 0 4 0 1 10 00 {}", SIGNERS[0])).as_deref(), Some(SIGNERS[0]));
    }
}
