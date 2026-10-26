#![windows_subsystem = "windows"]

mod accounts;
mod app_settings;
mod context_menu;
mod dashboard;
mod desktop_compositor;
mod diagnose;
mod font_catalog;
#[cfg(test)]
mod https_test;
mod known_folders;
mod localization;
mod manager;
mod models;
mod native_interop;
mod poller;
mod providers;
mod studio_app;
mod theme;
mod theme_engine;
mod theme_package;
mod tray_icon;
mod ui;
mod updater;
mod window;
mod winsqlite;

fn main() {
    diagnose::install_panic_hook();

    let args: Vec<String> = std::env::args().collect();
    // Management commands validate roots and emit JSON errors without ever
    // entering the GUI or showing a modal dialog.
    if let Some(exit_code) = manager::cli::handle_manage_mode(&args) {
        std::process::exit(exit_code);
    }
    if let Err(error) = manager::paths::validate_root_overrides() {
        manager::native::show_error_dialog(
            "UsageHub",
            &format!("Invalid UsageHub root override: {error}"),
        );
        std::process::exit(2);
    }
    let diagnose_enabled = args.iter().any(|arg| arg == "--diagnose");
    if diagnose_enabled {
        let init_result = if args.iter().any(|arg| arg == "--diagnose-append") {
            diagnose::init_append()
        } else {
            diagnose::init()
        };
        match init_result {
            Ok(path) => diagnose::log(format!("startup mode=runtime log_path={}", path.display())),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
    }

    if studio_app::handle_cli_mode(&args) {
        return;
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        if diagnose_enabled {
            diagnose::log(format!("cli mode exited with code {exit_code}"));
        }
        std::process::exit(exit_code);
    }

    if diagnose_enabled {
        diagnose::log("entering window::run");
    }
    window::run();
}
