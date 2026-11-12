//! Search: the apps that match a query, best matches first.

use crate::model::App;
use std::collections::BTreeSet;

/// How well `name` (lowercase) matches `needle`: all of it, its start, the start of one
/// of its words, or anywhere in it.
fn name_score(name: &str, needle: &str) -> u32 {
    if name == needle {
        1000
    } else if name.starts_with(needle) {
        800
    } else if name.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(needle)) {
        600
    } else if name.contains(needle) {
        400
    } else {
        0
    }
}

/// How well one needle matches an app: its name first, then a name it answers to
/// (package, aliases, command), then what it says about itself.
fn needle_score(app: &App, name: &str, needle: &str) -> u32 {
    let by_name = name_score(name, needle);
    if by_name > 0 {
        return by_name;
    }
    let names: Vec<String> =
        app.keywords.iter().chain(std::iter::once(&app.package)).map(|n| n.to_lowercase()).collect();
    if names.iter().any(|n| n == needle) {
        300
    } else if names.iter().any(|n| n.contains(needle)) {
        200
    } else if format!("{} {}", app.summary, app.category).to_lowercase().contains(needle) {
        100
    } else {
        0
    }
}

/// How well `app` matches `query`, or `None` when a word of it matches nothing. The
/// whole query matching the name beats its words matching one by one.
pub fn score(app: &App, query: &str) -> Option<u32> {
    let query = query.trim().to_lowercase();
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        return Some(0);
    }
    let name = app.name.to_lowercase();
    let mut weakest = u32::MAX;
    for word in &words {
        let s = needle_score(app, &name, word);
        if s == 0 {
            return None;
        }
        weakest = weakest.min(s);
    }
    let whole = if words.len() > 1 { name_score(&name, &query) } else { 0 };
    Some(whole.max(weakest))
}

/// The apps matching `query`, best first; ties go to Nyx apps, apps with a window,
/// popular ones and installed ones, then by name. With `dedupe`, an app another match
/// already offers (in its `also`) is left out.
pub fn rank<'a>(apps: impl IntoIterator<Item = &'a App>, query: &str, dedupe: bool) -> Vec<&'a App> {
    let mut found: Vec<(u32, &App)> = apps.into_iter().filter_map(|a| score(a, query).map(|s| (s, a))).collect();
    if dedupe {
        let offered: BTreeSet<(&str, &str)> =
            found.iter().flat_map(|(_, a)| a.also.iter().map(|(s, p)| (s.as_str(), p.as_str()))).collect();
        found.retain(|(_, a)| !offered.contains(&(a.source.as_str(), a.package.as_str())));
    }
    found.sort_by_cached_key(|(s, a)| {
        (std::cmp::Reverse(*s), !a.nyx, !a.desktop, !a.popular, a.installed.is_none(), a.name.to_lowercase())
    });
    let mut seen = BTreeSet::new();
    found.into_iter().filter(|(_, a)| seen.insert((a.source.clone(), a.package.clone()))).map(|(_, a)| a).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(source: &str, package: &str, name: &str, summary: &str) -> App {
        App {
            source: source.into(),
            package: package.into(),
            name: name.into(),
            summary: summary.into(),
            desktop: true,
            ..Default::default()
        }
    }

    fn names(apps: &[&App]) -> Vec<String> {
        apps.iter().map(|a| a.name.clone()).collect()
    }

    #[test]
    fn names_beat_summaries() {
        let apps = [
            app("flathub", "org.example.Burner", "Burner", "Burn discs; a Firefox companion"),
            app("flathub", "org.example.Foxy", "Firefox Sync Tool", "Sync bookmarks"),
            app("nyxos", "firefox", "Firefox", "A private web browser"),
            app("flathub", "org.example.Fire", "Campfire", "Chat around the fire"),
        ];
        assert_eq!(names(&rank(&apps, "firefox", false)), ["Firefox", "Firefox Sync Tool", "Burner"]);
        assert_eq!(names(&rank(&apps, "fire", false)), ["Firefox", "Firefox Sync Tool", "Campfire", "Burner"]);
    }

    #[test]
    fn every_word_has_to_match() {
        let apps = [
            app("flathub", "a", "Image Viewer", "Look at photos"),
            app("flathub", "b", "Image Editor", "Edit photos"),
            app("flathub", "c", "Text Editor", "Edit text"),
        ];
        assert_eq!(names(&rank(&apps, "image editor", false)), ["Image Editor"]);
        assert_eq!(names(&rank(&apps, "edit photos", false)), ["Image Editor"]);
        assert!(rank(&apps, "spreadsheet", false).is_empty());
    }

    #[test]
    fn keywords_and_packages_count() {
        let mut agent = app("nyxos", "claude-code", "Claude Code", "A coding agent");
        agent.keywords = vec!["claude".into(), "claude-code".into()];
        let apps = [agent, app("nixpkgs", "ripgrep", "rg", "Search files")];
        assert_eq!(names(&rank(&apps, "ripgrep", false)), ["rg"]);
        assert_eq!(names(&rank(&apps, "claude", false)), ["Claude Code"]);
    }

    #[test]
    fn what_another_source_offers_shows_once() {
        let mut gimp = app("nyxos", "gimp", "GIMP", "Edit photos");
        gimp.also = vec![("flathub".into(), "org.gimp.GIMP".into())];
        let apps = [app("flathub", "org.gimp.GIMP", "GIMP", "Edit photos"), gimp];
        let all = rank(&apps, "gimp", true);
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].source, "nyxos");
        assert_eq!(rank(&apps, "gimp", false).len(), 2);
    }

    #[test]
    fn ties_go_to_nyx_apps_then_installed() {
        let mut nyx = app("nyxos", "nyx-notes", "Notes", "Write notes");
        nyx.nyx = true;
        let mut installed = app("flathub", "org.example.Notes", "Notes", "Write notes");
        installed.installed = Some("1".into());
        let other = app("flathub", "org.other.Notes", "Notes", "Write notes");
        let apps = [other, installed, nyx];
        let found = rank(&apps, "notes", false);
        assert_eq!(found[0].package, "nyx-notes");
        assert_eq!(found[1].package, "org.example.Notes");
    }
}
