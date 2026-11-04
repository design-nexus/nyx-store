use crate::{
    model::{Change, Event, Plan, Request, protected, valid_name},
    packages,
    provenance::Provenance,
    repositories,
};
use alpm::{Alpm, SigLevel, TransFlag};
use anyhow::{Result, bail, ensure};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

pub fn emit(event: Event) {
    // A closed window must not abort a commit from inside a libalpm callback.
    let mut out = std::io::stdout().lock();
    if let Ok(json) = serde_json::to_string(&event) {
        let _ = writeln!(out, "{json}");
        let _ = out.flush();
    }
}

pub fn validate_change(change: &Change, known: Option<&str>, installed: bool, preferred: Option<&str>) -> Result<()> {
    if installed {
        let source = known
            .ok_or_else(|| anyhow::anyhow!("{}: review its original source in Repositories before updating", change.name))?;
        ensure!(source == change.source, "{}: blocked source change from {source} to {}", change.name, change.source);
    } else if preferred == Some("omarchy") {
        ensure!(change.source == "omarchy", "{}: Omarchy has priority over {}", change.name, change.source);
    }
    Ok(())
}

fn prepare(h: &mut Alpm, request: &Request, sources: &BTreeMap<String, (String, String)>) -> Result<Plan> {
    let before = packages::installed(h);
    match request {
        Request::Remove { package } => {
            ensure!(valid_name(package) && !protected(package), "This system component cannot be removed in Store");
            ensure!(!repositories::system_config()?.hold_pkg.contains(package), "This package is protected by pacman.conf");
            let p = h.localdb().pkg(package.as_str())?;
            ensure!(
                !h.syncdbs().iter().filter(|d| d.name() == "core").any(|d| d.pkg(package.as_str()).is_ok()),
                "Core system packages cannot be removed in Store"
            );
            h.trans_remove_pkg(p)?;
        }
        Request::Install { source, packages: names } => {
            ensure!(!names.is_empty() && names.len() < 300, "Invalid package selection");
            add_upgrades(h, sources)?;
            for name in names {
                ensure!(valid_name(name), "Invalid package name");
                let p = packages::pkg(h, source, name)?;
                ensure!(!p.should_ignore(), "{name} is ignored in pacman.conf");
                if !h.trans_add().iter().any(|a| a.name() == name) {
                    h.trans_add_pkg(p).map_err(|e| anyhow::anyhow!("Cannot add {name}: {e:?}"))?;
                } else {
                    ensure!(
                        h.trans_add().iter().find(|a| a.name() == name).and_then(|a| a.db()).is_some_and(|d| d.name() == source),
                        "{name}: the selected source differs from its update source"
                    );
                }
            }
        }
        Request::Update | Request::AurInstall { .. } => {
            add_upgrades(h, sources)?;
        }
        _ => bail!("This request is not a package transaction"),
    }
    h.trans_prepare().map_err(|e| anyhow::anyhow!("Cannot resolve this transaction: {e:?}"))?;
    let mut changes = Vec::new();
    for p in h.trans_add() {
        let source = if matches!(request, Request::AurInstall { .. }) && p.db().is_none() {
            "aur"
        } else {
            p.db().map(|d| d.name()).unwrap_or("local")
        };
        let old = before.get(p.name());
        let known = sources.get(p.name()).filter(|(_, stamp)| old.is_some_and(|p| &p.stamp == stamp)).map(|(s, _)| s.as_str());
        let c = Change {
            name: p.name().into(),
            old: old.map(|p| p.version.clone()),
            new: Some(p.version().to_string()),
            source: source.into(),
            bytes: p.size(),
        };
        validate_change(&c, known, old.is_some(), packages::preferred(h, p.name()).and_then(|p| p.db()).map(|d| d.name()))?;
        changes.push(c);
    }
    for p in h.trans_remove() {
        ensure!(
            matches!(request, Request::Remove { package } if package == p.name()),
            "{} would be removed or replaced; Store blocks implicit removals",
            p.name()
        );
        ensure!(!protected(p.name()), "Cannot remove an Omarchy system component");
        changes.push(Change {
            name: p.name().into(),
            old: Some(p.version().to_string()),
            new: None,
            source: sources.get(p.name()).map(|p| p.0.clone()).unwrap_or_else(|| "unknown".into()),
            bytes: 0,
        });
    }
    changes.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Plan { changes, snapshot: packages::snapshot(h), notes: Vec::new() })
}

fn add_upgrades(h: &Alpm, sources: &BTreeMap<String, (String, String)>) -> Result<()> {
    for old in h.localdb().pkgs() {
        if old.should_ignore() {
            continue;
        }
        let known = sources.get(old.name()).filter(|(_, s)| *s == packages::stamp(old)).map(|(s, _)| s.as_str());
        if let Some(source) = known {
            if source == "aur" || source == "local" {
                continue;
            }
            let new = packages::pkg(h, source, old.name()).map_err(|_| {
                anyhow::anyhow!(
                    "{}: original source {source} no longer provides this package; no source migration will be performed",
                    old.name()
                )
            })?;
            if new.version() > old.version() {
                h.trans_add_pkg(new).map_err(|e| anyhow::anyhow!("Cannot upgrade {}: {e:?}", old.name()))?;
            }
        } else if packages::preferred(h, old.name()).is_some_and(|p| p.version() > old.version()) {
            bail!("{}: its source is unconfirmed. Review existing sources in Repositories first.", old.name());
        }
    }
    Ok(())
}

fn confirm(plan: &Plan, input: &mut impl BufRead) -> Result<()> {
    emit(Event::Plan { plan: plan.clone() });
    let mut line = String::new();
    input.take(2 * 1024 * 1024).read_line(&mut line)?;
    ensure!(!line.is_empty(), "Transaction cancelled before installation");
    let approved: Plan = serde_json::from_str(&line)?;
    ensure!(&approved == plan, "The approved transaction differs from the prepared transaction");
    Ok(())
}

pub fn run(request: Request, input: &mut impl BufRead) -> Result<()> {
    // Only root-owned state is trusted by the helper. The installer creates its parents.
    let state = Path::new(repositories::STATE);
    fs::create_dir_all(state)?;
    fs::set_permissions(state, fs::Permissions::from_mode(0o755))?;

    let mut provenance = Provenance::open(&packages::provenance_path())?;
    // Reconcile before refreshing: interrupted commits can leave only part of a plan installed.
    let mut h = packages::handle(state, true, false)?;
    h.trans_init(TransFlag::NONE)
        .map_err(|e| anyhow::anyhow!("Cannot lock pacman’s database: {e}. Close other package managers and retry."))?;
    packages::recover_legacy_local()?;
    provenance.reconcile(&packages::installed(&h))?;
    match &request {
        Request::BlackarchSetup => {
            crate::blackarch::setup(&mut h, &mut provenance)?;
            return Ok(());
        }
        Request::Sources { assignments } => {
            ensure!(assignments.len() <= 10000, "Too many source assignments");
            let installed = packages::installed(&h);
            let existing = Provenance::sources(&packages::provenance_path())?;
            for (name, source, _) in assignments {
                ensure!(valid_name(name), "Invalid package name");
                ensure!(
                    !existing.contains_key(name),
                    "{name} already has a recorded source; source migrations are not supported"
                );
                ensure!(
                    matches!(source.as_str(), "aur" | "local") || packages::pkg(&h, source, name).is_ok(),
                    "{name} is unavailable in {source}"
                );
            }
            provenance.assign(assignments, &installed)?;
            emit(Event::Done { message: "Original sources saved".into() });
            return Ok(());
        }
        Request::Repository { repository } => {
            repositories::save(repository.clone())?;
            emit(Event::Done { message: "Repository saved. Refresh the catalog to load it.".into() });
            return Ok(());
        }
        Request::TrustKey { fingerprint } => {
            ensure!(
                fingerprint.len() == 40 && fingerprint.bytes().all(|c| c.is_ascii_hexdigit()),
                "Enter a full 40-character signing-key fingerprint"
            );
            for args in [
                vec!["--keyserver", "hkps://keyserver.ubuntu.com", "--recv-keys", fingerprint.as_str()],
                vec!["--lsign-key", fingerprint.as_str()],
            ] {
                let output = Command::new("/usr/bin/pacman-key").args(args).output()?;
                ensure!(output.status.success(), "Signing-key setup failed: {}", String::from_utf8_lossy(&output.stderr));
            }
            emit(Event::Done { message: "Signing key imported and trusted".into() });
            return Ok(());
        }
        _ => {}
    }
    h.trans_release()?;
    drop(h);
    emit(Event::Status { message: "Refreshing signed repository metadata…".into() });
    h = packages::handle(state, true, true)?;
    let sources = Provenance::sources(&packages::provenance_path())?;
    h.trans_init(TransFlag::NONE)?;
    provenance.reconcile(&packages::installed(&h))?;
    let staging = state.join("artifacts");
    let result = (|| -> Result<()> {
        if let Request::AurInstall { artifacts, base, commit } = &request {
            ensure!(
                valid_name(base) && commit.len() == 40 && commit.bytes().all(|c| c.is_ascii_hexdigit()),
                "Invalid AUR build identity"
            );
            ensure!(!artifacts.is_empty() && artifacts.len() <= 100, "Invalid AUR artifacts");
            fs::create_dir_all(&staging)?;
            fs::set_permissions(&staging, fs::Permissions::from_mode(0o700))?;
            for (i, artifact) in artifacts.iter().enumerate() {
                let mut from = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(artifact)?;
                ensure!(
                    from.metadata()?.is_file() && from.metadata()?.len() < 8 * 1024 * 1024 * 1024u64,
                    "Invalid build artifact"
                );
                let dest = staging.join(format!("build-{i}.pkg.tar.zst"));
                let mut to = File::create(&dest)?;
                std::io::copy(&mut from, &mut to)?;
                to.sync_all()?;
                let pkg = h.pkg_load(dest.to_string_lossy().as_bytes(), true, SigLevel::NONE)?;
                ensure!(pkg.base() == Some(base.as_str()), "AUR artifact does not belong to {base}");
                ensure!(!protected(pkg.name()), "AUR cannot replace protected system packages");
                h.trans_add_pkg(pkg).map_err(|e| anyhow::anyhow!("Cannot stage AUR artifact: {e:?}"))?;
            }
        }
        let plan = prepare(&mut h, &request, &sources)?;
        if plan.changes.is_empty() {
            emit(Event::Done { message: "Everything is up to date".into() });
            return Ok(());
        }
        confirm(&plan, input)?;
        ensure!(packages::snapshot(&h) == plan.snapshot, "Installed packages changed; review a fresh transaction");
        provenance.begin(&plan)?;
        // The journal is durable before libalpm runs hooks or changes any package.
        let audit = OpenOptions::new().create(true).append(true).mode(0o644).open(state.join("history.jsonl"))?;
        serde_json::to_writer(&audit, &request)?;
        writeln!(&audit)?;
        audit.sync_all()?;
        h.set_progress_cb((), |_, package, percent, _, _, _| {
            emit(Event::Status { message: format!("Installing {package} · {percent}%") })
        });
        h.set_event_cb((), |event, _| {
            if let alpm::Event::ScriptletInfo(info) = event.event() {
                emit(Event::Status { message: info.line().into() });
            }
        });
        emit(Event::Status { message: "Installing the reviewed transaction. Keep Store open until it finishes.".into() });
        let commit =
            h.trans_commit().map_err(|e| anyhow::anyhow!("Installation failed: {e:?}. Refresh to reconcile installed packages."));
        h.trans_release()?;
        // A fresh handle observes partially committed on-disk package state as well.
        drop(std::mem::replace(&mut h, packages::handle(state, true, false)?));
        provenance.reconcile(&packages::installed(&h))?;
        commit?;
        emit(Event::Done { message: "Transaction completed".into() });
        Ok(())
    })();
    let _ = h.trans_release();
    if staging.exists() {
        for entry in fs::read_dir(&staging)?.flatten() {
            if entry.file_type()?.is_file() {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
    result
}

pub fn start(request: Request, send: async_channel::Sender<Event>, confirmation: std::sync::mpsc::Receiver<Option<Plan>>) {
    if crate::nix::active() {
        return crate::nix::start(request, send, confirmation);
    }
    let result = (|| -> Result<()> {
        ensure!(
            Path::new(repositories::HELPER).exists(),
            "Store's system helper is not installed. Run ./install.sh store from the Nyx checkout."
        );
        let mut child = Command::new("/usr/bin/pkexec")
            .arg(repositories::HELPER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", serde_json::to_string(&request)?)?;
        let stderr = child.stderr.take().unwrap();
        let errors = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = BufReader::new(stderr).read_to_string(&mut s);
            s
        });
        let mut terminal = None;
        for line in BufReader::new(child.stdout.take().unwrap()).lines() {
            let line = line?;
            let Ok(event) = serde_json::from_str::<Event>(&line) else { continue };
            let needs_confirmation = matches!(event, Event::Plan { .. });
            let finished = matches!(event, Event::Done { .. } | Event::Error { .. });
            if finished {
                terminal = Some(event);
                break;
            }
            if send.send_blocking(event).is_err() {
                break;
            }
            if needs_confirmation {
                match confirmation.recv() {
                    Ok(Some(plan)) => writeln!(stdin, "{}", serde_json::to_string(&plan)?)?,
                    _ => {
                        drop(stdin);
                        let _ = child.wait();
                        return Ok(());
                    }
                }
            }
        }
        drop(stdin);
        let status = child.wait()?;
        let error = errors.join().unwrap_or_default();
        // Announce completion only after the helper releases both package locks.
        if let Some(event) = terminal {
            let _ = send.send_blocking(event);
            return Ok(());
        }
        ensure!(
            status.success(),
            "{}",
            if error.trim().is_empty() { "Authorization or package operation was cancelled" } else { error.trim() }
        );
        Ok(())
    })();
    if let Err(e) = result {
        let _ = send.send_blocking(Event::Error { message: e.to_string() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn change(source: &str) -> Change {
        Change { name: "example".into(), old: Some("1".into()), new: Some("2".into()), source: source.into(), bytes: 0 }
    }
    #[test]
    fn source_policy() {
        assert!(validate_change(&change("extra"), Some("extra"), true, Some("omarchy")).is_ok());
        assert!(validate_change(&change("omarchy"), Some("extra"), true, Some("omarchy")).is_err());
        assert!(validate_change(&change("extra"), None, true, Some("extra")).is_err());
        assert!(validate_change(&change("extra"), None, false, Some("omarchy")).is_err());
        assert!(validate_change(&change("blackarch"), Some("aur"), true, None).is_err());
    }
    #[test]
    fn approval_is_exact() {
        let plan = Plan { snapshot: "state-a".into(), ..Default::default() };
        let mut stale = plan.clone();
        stale.snapshot = "state-b".into();
        assert!(confirm(&plan, &mut std::io::Cursor::new(serde_json::to_vec(&stale).unwrap())).is_err());
        assert!(confirm(&plan, &mut std::io::Cursor::new(Vec::<u8>::new())).is_err());
    }
}
