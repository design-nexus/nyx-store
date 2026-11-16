use crate::model::{App, valid_name};
use anyhow::{Result, ensure};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Response {
    #[serde(default)]
    results: Vec<Package>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Package {
    name: String,
    #[serde(default)]
    description: Option<String>,
    version: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    out_of_date: Option<i64>,
}

pub fn search(query: &str) -> Result<Vec<App>> {
    if query.chars().count() < 2 {
        return Ok(Vec::new());
    }
    let response: Response = ureq::get("https://aur.archlinux.org/rpc/")
        .query("v", "5")
        .query("type", "search")
        .query("arg", query)
        .query("by", "name-desc")
        .call()?
        .body_mut()
        .read_json()?;
    ensure!(response.error.is_none(), "AUR: {}", response.error.unwrap_or_default());
    Ok(response
        .results
        .into_iter()
        .take(200)
        .map(|p| App {
            package: p.name.clone(),
            name: p.name,
            summary: format!(
                "{}{}",
                p.description.unwrap_or_default(),
                if p.out_of_date.is_some() { " · Flagged out of date" } else { "" }
            ),
            version: p.version,
            website: p.url.unwrap_or_default(),
            source: "aur".into(),
            category: "AUR".into(),
            ..Default::default()
        })
        .collect())
}

#[derive(Clone)]
pub struct Recipe {
    pub base: String,
    pub commit: String,
    pub directory: PathBuf,
    pub review: String,
    pub dependencies: Vec<String>,
}

fn command(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("/usr/bin/git").arg("-c").arg("core.hooksPath=/dev/null").arg("-C").arg(dir).args(args).output()?;
    ensure!(output.status.success(), "AUR checkout failed: {}", String::from_utf8_lossy(&output.stderr));
    Ok(String::from_utf8(output.stdout)?)
}

pub fn recipe(cache: &Path, package: &str) -> Result<Recipe> {
    ensure!(valid_name(package), "Invalid AUR package name");
    let value: serde_json::Value =
        ureq::get("https://aur.archlinux.org/rpc/v5/info").query("arg[]", package).call()?.body_mut().read_json()?;
    let base = value["results"][0]["PackageBase"].as_str().ok_or_else(|| anyhow::anyhow!("AUR package was removed"))?.to_string();
    ensure!(valid_name(&base), "Invalid AUR package base");
    let directory = cache.join("aur").join(&base);
    if !directory.exists() {
        fs::create_dir_all(directory.parent().unwrap())?;
        let out = Command::new("/usr/bin/git")
            .args(["-c", "core.hooksPath=/dev/null", "clone", "--", &format!("https://aur.archlinux.org/{base}.git")])
            .arg(&directory)
            .output()?;
        ensure!(out.status.success(), "Cannot download the AUR recipe: {}", String::from_utf8_lossy(&out.stderr));
    } else {
        command(&directory, &["fetch", "--", "origin", "master"])?;
        // No local recipe edits are discarded; a dirty checkout requires user attention.
        ensure!(
            command(&directory, &["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
            "The AUR recipe has local changes. Move the build folder aside before refreshing."
        );
        command(&directory, &["merge", "--ff-only", "FETCH_HEAD"])?;
    }
    let commit = command(&directory, &["rev-parse", "HEAD"])?.trim().to_string();
    let srcinfo = fs::read_to_string(directory.join(".SRCINFO"))?;
    let mut dependencies = Vec::new();
    for line in srcinfo.lines() {
        if let Some((key, value)) = line.trim().split_once(" = ")
            && matches!(
                key,
                "depends" | "makedepends" | "checkdepends" | "depends_x86_64" | "makedepends_x86_64" | "checkdepends_x86_64"
            )
        {
            dependencies.push(value.to_string());
        }
    }
    dependencies.sort();
    dependencies.dedup();
    let files = command(&directory, &["ls-files"])?;
    let mut review = format!(
        "AUR base: {base}\nCommit: {commit}\n\nBuild scripts run as your user and can access your files. Review every recipe before building.\n"
    );
    for name in files.lines() {
        let data = command(&directory, &["show", &format!("HEAD:{name}")])?;
        review.push_str(&format!("\n──── {name} ────\n{data}\n"));
    }
    Ok(Recipe { base, commit, directory, review, dependencies })
}

pub fn missing_dependencies(recipe: &Recipe, cache: &Path) -> Result<Vec<(String, String)>> {
    let h = crate::packages::handle(cache, false, false)?;
    let mut needed = Vec::new();
    for dependency in &recipe.dependencies {
        if h.localdb().pkgs().find_satisfier(dependency.as_str()).is_some() {
            continue;
        }
        let p = h.syncdbs().find_satisfier(dependency.as_str()).ok_or_else(|| anyhow::anyhow!("AUR dependency {dependency} must be installed first. Find it in Store, review and install it, then retry this build."))?;
        needed.push((p.db().unwrap().name().into(), p.name().into()));
    }
    Ok(needed)
}

pub fn build(recipe: &Recipe, cancel: &std::sync::atomic::AtomicBool, progress: impl Fn(String)) -> Result<Vec<String>> {
    use std::{
        io::{Read, Seek, SeekFrom},
        os::unix::process::CommandExt,
        process::Stdio,
        sync::atomic::Ordering,
    };
    ensure!(unsafe { libc::geteuid() } != 0, "AUR builds must run without root privileges");
    ensure!(command(&recipe.directory, &["rev-parse", "HEAD"])?.trim() == recipe.commit, "The reviewed AUR commit changed");
    ensure!(
        command(&recipe.directory, &["status", "--porcelain", "--untracked-files=no"])?.is_empty(),
        "The reviewed recipe has changed"
    );
    let log_path = recipe.directory.join("nyx-build.log");
    let log = fs::File::create(&log_path)?;
    let mut child = Command::new("/usr/bin/makepkg")
        .current_dir(&recipe.directory)
        .args(["--cleanbuild", "--force", "--noconfirm"])
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    let mut length = 0;
    let status = loop {
        if cancel.load(Ordering::Relaxed) {
            // This is an unprivileged build process group, never a package commit.
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGTERM);
            }
            for _ in 0..10 {
                if child.try_wait()?.is_some() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.wait();
            anyhow::bail!("AUR build cancelled. Build log: {}", log_path.display());
        }
        let size = fs::metadata(&log_path)?.len();
        if size != length {
            length = size;
            let mut tail = fs::File::open(&log_path)?;
            tail.seek(SeekFrom::Start(size.saturating_sub(2048)))?;
            let mut bytes = Vec::new();
            tail.read_to_end(&mut bytes)?;
            if let Some(line) = String::from_utf8_lossy(&bytes).lines().rev().find(|s| !s.trim().is_empty()) {
                progress(line.chars().take(240).collect());
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(400));
    };
    ensure!(status.success(), "AUR build failed. See {}", log_path.display());
    let output = Command::new("/usr/bin/makepkg").current_dir(&recipe.directory).arg("--packagelist").output()?;
    ensure!(output.status.success(), "Cannot locate built packages");
    let files: Vec<_> =
        String::from_utf8(output.stdout)?.lines().filter(|p| Path::new(p).is_file()).map(str::to_string).collect();
    ensure!(!files.is_empty(), "The AUR build produced no packages");
    Ok(files)
}

pub fn info(names: &[String]) -> Result<Vec<App>> {
    let mut apps = Vec::new();
    for batch in names.chunks(100) {
        let mut request = ureq::get("https://aur.archlinux.org/rpc/v5/info");
        for name in batch {
            request = request.query("arg[]", name);
        }
        let response: Response = request.call()?.body_mut().read_json()?;
        ensure!(response.error.is_none(), "AUR: {}", response.error.unwrap_or_default());
        apps.extend(response.results.into_iter().map(|p| App {
            package: p.name.clone(),
            name: p.name,
            summary: p.description.unwrap_or_default(),
            version: p.version,
            website: p.url.unwrap_or_default(),
            source: "aur".into(),
            ..Default::default()
        }));
    }
    Ok(apps)
}
