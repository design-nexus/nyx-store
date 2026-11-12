mod prefs;
mod theme;
mod widgets;
mod window;
use gtk::prelude::*;
use gtk::{gio, glib};
fn main() -> glib::ExitCode {
    nyx_core::app::init(nyx_core::app::App { slug: "nyx-store", name: "Store" });
    if std::env::args().any(|a| a == "--help") {
        println!(
            "Usage: nyx-store [--section home|nyx|categories|installed|updates|repositories] [--search TEXT] [--remove DESKTOP-ID]\n\
             \n  --remove DESKTOP-ID  show the installed app a launcher belongs to, to remove it"
        );
        return glib::ExitCode::SUCCESS;
    }
    if std::env::args().any(|a| a == "--blackarch-check") {
        return match nyx_store::blackarch::verify_bootstrap(
            &nyx_core::paths::cache_dir().join("blackarch-verification"),
            |message| println!("{message}"),
        ) {
            Ok(verified) => {
                println!("Verified BlackArch keyring signed by {}", verified.fingerprint);
                glib::ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("{error}");
                glib::ExitCode::FAILURE
            }
        };
    }
    if std::env::args().any(|a| a == "--artwork-check") {
        let result = (|| -> anyhow::Result<()> {
            let cache = nyx_core::paths::cache_dir();
            let catalog = nyx_store::catalog::load(&cache, false)?;
            for name in ["blender", "krita", "obs-studio", "firefox"] {
                let app =
                    catalog.apps.iter().find(|a| a.package == name).ok_or_else(|| anyhow::anyhow!("Missing app: {name}"))?;
                let icon = nyx_store::assets::icon(&cache, &app.icon)?;
                let screenshot = nyx_store::assets::screenshot(&cache, app, 0)?;
                println!("{name}: decoded icon {} and screenshot {}", icon.display(), screenshot.display());
            }
            Ok(())
        })();
        return match result {
            Ok(()) => glib::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                glib::ExitCode::FAILURE
            }
        };
    }
    // Nyx Assistant's search: the matching apps as JSON, read from the local package
    // databases (no window, no network, nothing installed).
    if let Some(i) = std::env::args().position(|a| a == "--find") {
        let query: String = std::env::args().skip(i + 1).collect::<Vec<_>>().join(" ").to_lowercase();
        return match nyx_store::catalog::load(&nyx_core::paths::cache_dir(), false) {
            Ok(c) => {
                let found = nyx_store::search::rank(&c.apps, &query, true);
                let apps: Vec<serde_json::Value> = found
                    .iter()
                    .take(15)
                    .map(|a| serde_json::json!({"package": a.package, "name": a.name, "summary": a.summary, "version": a.version, "installed": a.installed, "source": a.source, "category": a.category}))
                    .collect();
                println!("{}", serde_json::json!({"apps": apps, "matches": found.len()}));
                glib::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                glib::ExitCode::FAILURE
            }
        };
    }
    if std::env::args().any(|a| a == "--catalog-check") {
        return match nyx_store::catalog::load(&nyx_core::paths::cache_dir(), std::env::args().any(|a| a == "--refresh")) {
            Ok(c) => {
                if !c.warning.is_empty() {
                    eprintln!("{}", c.warning);
                    return glib::ExitCode::FAILURE;
                }
                println!(
                    "{} packages; {} desktop entries; {} popular picks",
                    c.apps.len(),
                    c.apps.iter().filter(|a| a.desktop).count(),
                    c.apps.iter().filter(|a| a.popular).count()
                );
                glib::ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                glib::ExitCode::FAILURE
            }
        };
    }
    if std::env::var_os("GSK_RENDERER").is_none() {
        unsafe { std::env::set_var("GSK_RENDERER", "ngl") };
    }
    // Developer aid: a different id gives a separate instance (a test copy on a virtual
    // monitor), so it doesn't hand its arguments to the copy that's open.
    let id = std::env::var("NYX_STORE_APP_ID").unwrap_or_else(|_| "io.github.design_nexus.nyx.Store".to_string());
    let app = gtk::Application::builder().application_id(id).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_command_line(|app, cli| {
        let args: Vec<_> = cli.arguments().iter().map(|a| a.to_string_lossy().to_string()).collect();
        let value = |key| args.iter().position(|a| a == key).and_then(|i| args.get(i + 1)).map(String::as_str);
        window::present(app, value("--section"), value("--search"), value("--remove"));
        glib::ExitCode::SUCCESS
    });
    app.connect_shutdown(|_| prefs::flush());
    app.run()
}
