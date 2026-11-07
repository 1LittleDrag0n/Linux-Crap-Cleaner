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

    // Background usage tracker (started by a systemd user service): no GUI.
    let args: Vec<String> = std::env::args().collect();
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
