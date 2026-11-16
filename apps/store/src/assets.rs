//! Decode artwork once into PNG so rendering does not depend on the GTK icon theme
//! having an app installed, or its image loaders supporting distribution JPEG XL.
use crate::model::App;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Cursor,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);

fn temporary(path: &Path) -> PathBuf {
    path.with_file_name(format!(
        ".{}-{}-{}.png",
        path.file_stem().unwrap_or_default().to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}
fn decode(bytes: &[u8]) -> Result<image::DynamicImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(128 * 1024 * 1024);
    limits.max_image_width = Some(8192);
    limits.max_image_height = Some(8192);
    reader.limits(limits);
    Ok(reader.decode()?)
}
fn cached_png(path: &Path) -> bool {
    fs::read(path).ok().is_some_and(|b| b.starts_with(b"\x89PNG\r\n\x1a\n"))
}
fn write_png(cache: &Path, key: &[u8], bytes: &[u8]) -> Result<PathBuf> {
    let path = cache.join(format!("{:x}.png", Sha256::digest(key)));
    if cached_png(&path) {
        return Ok(path);
    }
    let image = decode(bytes)?;
    let temp = temporary(&path);
    image.thumbnail(1800, 1200).save_with_format(&temp, image::ImageFormat::Png)?;
    fs::rename(temp, &path)?;
    Ok(path)
}

pub fn icon(cache: &Path, icon: &str) -> Result<PathBuf> {
    let relative = icon.strip_prefix("@cache:").context("No distribution icon")?;
    ensure!(!relative.starts_with('/') && !relative.split('/').any(|s| s == ".."), "Invalid icon path");
    let source = [
        cache.join("swicons").join(relative),
        Path::new("/usr/share/swcatalog/icons").join(relative),
        cache.join("swicons").join(relative.replace("/128x128/", "/64x64/")),
    ]
    .into_iter()
    .find(|p| p.is_file())
    .context("Distribution icon is missing")?;
    let bytes = fs::read(&source)?;
    let directory = cache.join("artwork/icons");
    fs::create_dir_all(&directory)?;
    let target = directory.join(format!("{:x}.png", Sha256::digest(&bytes)));
    if cached_png(&target) {
        return Ok(target);
    }
    if source.extension().is_some_and(|e| e == "jxl") {
        let temp = temporary(&target);
        let result = Command::new("/usr/bin/djxl")
            .arg(&source)
            .arg(&temp)
            .args(["--quiet", "--num_threads=1"])
            .output()
            .context("JPEG XL support is missing; install libjxl")?;
        ensure!(result.status.success(), "Cannot decode distribution icon: {}", String::from_utf8_lossy(&result.stderr));
        ensure!(cached_png(&temp), "Icon decoder produced no image");
        fs::rename(temp, &target)?;
        Ok(target)
    } else {
        write_png(&directory, &bytes, &bytes)
    }
}

fn download(url: &str) -> Result<Vec<u8>> {
    ensure!(url.starts_with("https://"), "Artwork URL must use HTTPS");
    Ok(ureq::get(url)
        .header("User-Agent", "Nyx-Store/0.1 (+https://github.com/design-nexus/nyx)")
        .config()
        .timeout_global(Some(std::time::Duration::from_secs(12)))
        .build()
        .call()?
        .body_mut()
        .with_config()
        .limit(20 * 1024 * 1024)
        .read_to_vec()?)
}

fn flathub_id(app: &App) -> Option<String> {
    let id = match app.package.as_str() {
        "blender" => "org.blender.Blender",
        "krita" => "org.kde.krita",
        "obs-studio" => "com.obsproject.Studio",
        "firefox" => "org.mozilla.firefox",
        "gimp" => "org.gimp.GIMP",
        "inkscape" => "org.inkscape.Inkscape",
        "vlc" => "org.videolan.VLC",
        "kdenlive" => "org.kde.kdenlive",
        "audacity" => "org.audacityteam.Audacity",
        _ => app.desktop_id.strip_suffix(".desktop").filter(|s| s.split('.').count() >= 3)?,
    };
    Some(id.to_string())
}
fn mirrored_screenshots(app: &App, cache: &Path, index: usize) -> Result<Vec<String>> {
    let id = flathub_id(app).context("No screenshot mirror for this app")?;
    ensure!(id.bytes().all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c)), "Invalid app identifier");
    let file = cache.join(format!("{id}.json"));
    let fresh =
        fs::metadata(&file).and_then(|m| m.modified()).ok().and_then(|m| m.elapsed().ok()).is_some_and(|d| d.as_secs() < 86400);
    let bytes = if fresh {
        fs::read(&file)?
    } else {
        let b = download(&format!("https://flathub.org/api/v2/appstream/{id}"))?;
        fs::write(&file, &b)?;
        b
    };
    let data: serde_json::Value = serde_json::from_slice(&bytes)?;
    let shots = data["screenshots"].as_array().context("No mirrored screenshots")?;
    let shot = shots.get(index).or_else(|| shots.first()).context("No mirrored screenshot")?;
    let mut sizes: Vec<_> = shot["sizes"].as_array().context("No screenshot sizes")?.iter().collect();
    sizes.sort_by_key(|v| v["width"].as_str().and_then(|s| s.parse::<i32>().ok()).unwrap_or(0).abs_diff(1248));
    Ok(sizes
        .iter()
        .filter_map(|v| v["src"].as_str())
        .filter(|u| u.starts_with("https://dl.flathub.org/"))
        .map(str::to_string)
        .collect())
}

pub fn screenshot(cache: &Path, app: &App, index: usize) -> Result<PathBuf> {
    ensure!(crate::model::valid_name(&app.package), "Invalid app package name");
    let directory = cache.join("artwork/screenshots");
    fs::create_dir_all(&directory)?;
    // Remember a working alternative, avoiding repeated failed requests to blocked hosts.
    let resolved = directory.join(format!("{}-{index}.url", app.package));
    let mut urls = Vec::new();
    if let Ok(url) = fs::read_to_string(&resolved)
        && url.starts_with("https://")
    {
        urls.push(url);
    }
    if let Some(url) = app.screenshots.get(index)
        && !urls.contains(url)
    {
        urls.push(url.clone());
    }
    let mut failures = Vec::new();
    for mirrored in [false, true] {
        if mirrored {
            urls = mirrored_screenshots(app, &directory, index)?;
        }
        for url in &urls {
            let target = directory.join(format!("{:x}.png", Sha256::digest(url.as_bytes())));
            if cached_png(&target) {
                return Ok(target);
            }
            let result = download(url).and_then(|bytes| write_png(&directory, url.as_bytes(), &bytes));
            match result {
                Ok(path) => {
                    fs::write(&resolved, url)?;
                    return Ok(path);
                }
                Err(error) => failures.push(format!("{url}: {error}")),
            }
        }
    }
    anyhow::bail!("No screenshot could load: {}", failures.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_error_pages_and_traversal() {
        assert!(decode(b"<html>Not found</html>").is_err());
        assert!(icon(Path::new("/tmp"), "@cache:../../etc/passwd").is_err());
    }
}
