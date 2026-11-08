//! Linux Crap Cleaner: a friendly system cleaner for Linux.

mod app;
mod pages;
mod widgets;

use tracing_subscriber::EnvFilter;

/// App-wide style tweaks on top of libadwaita.
const STYLE: &str = "
/* Disk usage bar when a disk is almost full. */
levelbar.almost-full block.filled,
levelbar.almost-full block:not(.empty) {
    background-color: @warning_bg_color;
}
/* Breadcrumb buttons use normal weight so the current folder stands out. */
button.breadcrumb { font-weight: normal; }
";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();

    // Inside an AppImage, pkexec runs the AppImage itself with this flag;
    // hand over to the bundled administrator helper next to this program.
    if args.get(1).map(String::as_str) == Some(lcc_core::privileged::HELPER_FLAG) {
        use std::os::unix::process::CommandExt;
        let helper = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|d| d.join("lcc-helper")));
        let Some(helper) = helper.filter(|h| h.is_file()) else {
            eprintln!("lcc-helper not found next to the app");
            std::process::exit(2);
        };
        let err = std::process::Command::new(helper).args(&args[2..]).exec();
        eprintln!("couldn't start lcc-helper: {err}");
        std::process::exit(2);
    }

    // Background usage tracker (started by a systemd user service): no GUI.
    if args.iter().any(|a| a == "--track-usage") {
        let once = args.iter().any(|a| a == "--once");
        if let Err(e) = lcc_core::usage::run_tracker(std::time::Duration::from_secs(60), once) {
            tracing::error!("usage tracker stopped: {e}");
            std::process::exit(1);
        }
        return;
    }

    let app = relm4::RelmApp::new(lcc_core::APP_ID);
    relm4::set_global_css(STYLE);
    app.run::<app::App>(());
}
