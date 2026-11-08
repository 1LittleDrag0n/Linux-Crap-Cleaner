//! Dashboard: disk usage, a quick look at the places junk builds up, and
//! facts about this computer.

use std::path::PathBuf;

use lcc_core::disks::{Disk, list_disks};
use lcc_core::format::{count, human_size, percent};
use lcc_core::scan::{ScanOptions, ScanProgress, scan};
use lcc_core::system::{PackageManager, SystemInfo};
use relm4::adw::prelude::*;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

/// Above this used fraction a disk's bar turns the warning color.
const ALMOST_FULL: f64 = 0.85;

/// A folder worth keeping an eye on.
struct QuickItem {
    title: &'static str,
    description: &'static str,
    path: PathBuf,
}

struct QuickRow {
    size: gtk::Label,
    spinner: gtk::Spinner,
    row: adw::ActionRow,
}

pub struct Dashboard {
    quick: Vec<QuickItem>,
}

#[derive(Debug)]
pub enum DashboardMsg {
    Refresh,
}

#[derive(Debug)]
pub enum DashboardOutput {
    /// Open this folder in the Storage Analyzer.
    Analyze(PathBuf),
}

#[derive(Debug)]
pub enum DashboardCmd {
    Disks(Vec<Disk>),
    QuickSize {
        index: usize,
        result: Result<(u64, u64, bool), String>,
    },
}

pub struct DashboardWidgets {
    disks_group: adw::PreferencesGroup,
    disk_rows: Vec<adw::ActionRow>,
    quick_rows: Vec<QuickRow>,
}

impl Component for Dashboard {
    type Init = SystemInfo;
    type Input = DashboardMsg;
    type Output = DashboardOutput;
    type CommandOutput = DashboardCmd;
    type Root = adw::PreferencesPage;
    type Widgets = DashboardWidgets;

    fn init_root() -> Self::Root {
        adw::PreferencesPage::new()
    }

    fn init(
        system: Self::Init,
        page: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Disks
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            refresh.connect_clicked(move |_| sender.input(DashboardMsg::Refresh));
        }
        let disks_group = adw::PreferencesGroup::builder()
            .title("Disks")
            .description("Click a disk to see what's using its space.")
            .header_suffix(&refresh)
            .build();
        page.add(&disks_group);

        // Quick look
        let quick = quick_items(&system);
        let quick_group = adw::PreferencesGroup::builder()
            .title("Quick Look")
            .description(
                "Places where junk usually builds up. Cleaning them comes with the Junk Cleaner.",
            )
            .build();
        let mut quick_rows = Vec::new();
        for item in &quick {
            let row = {
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .subtitle_lines(2)
                    .activatable(true)
                    .build();
                row.set_title(item.title);
                row.set_subtitle(&format!("{} · {}", item.description, item.path.display()));
                row
            };
            let spinner = gtk::Spinner::builder().spinning(true).build();
            let size = gtk::Label::builder().css_classes(["numeric"]).build();
            row.add_suffix(&spinner);
            row.add_suffix(&size);
            row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
            {
                let sender = sender.clone();
                let path = item.path.clone();
                row.connect_activated(move |_| {
                    let _ = sender.output(DashboardOutput::Analyze(path.clone()));
                });
            }
            quick_group.add(&row);
            quick_rows.push(QuickRow { size, spinner, row });
        }
        if quick.is_empty() {
            quick_group.add(&adw::ActionRow::builder().title("Nothing to show").build());
        }
        page.add(&quick_group);

        page.add(&system_group(&system));
        page.add(&safety_group());

        let model = Dashboard { quick };
        let widgets = DashboardWidgets {
            disks_group,
            disk_rows: Vec::new(),
            quick_rows,
        };
        model.refresh(&widgets, &sender);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        w: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            DashboardMsg::Refresh => self.refresh(w, &sender),
        }
    }

    fn update_cmd_with_view(
        &mut self,
        w: &mut Self::Widgets,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        _root: &Self::Root,
    ) {
        match msg {
            DashboardCmd::Disks(disks) => {
                for row in w.disk_rows.drain(..) {
                    w.disks_group.remove(&row);
                }
                if disks.is_empty() {
                    let row = adw::ActionRow::builder().title("No disks found").build();
                    w.disks_group.add(&row);
                    w.disk_rows.push(row);
                }
                for disk in disks {
                    let row = disk_row(&disk, &sender);
                    w.disks_group.add(&row);
                    w.disk_rows.push(row);
                }
            }
            DashboardCmd::QuickSize { index, result } => {
                let Some(q) = w.quick_rows.get(index) else {
                    return;
                };
                q.spinner.set_visible(false);
                match result {
                    Ok((size, files, partial)) => {
                        q.size.set_label(&if partial {
                            format!("at least {}", human_size(size))
                        } else {
                            human_size(size)
                        });
                        q.row.set_tooltip_text(Some(&if partial {
                            format!(
                                "{} files. Some folders need administrator access to measure.",
                                count(files)
                            )
                        } else {
                            format!("{} files", count(files))
                        }));
                    }
                    Err(e) => {
                        q.size.set_label("Unknown");
                        q.row.set_tooltip_text(Some(&e));
                    }
                }
            }
        }
    }
}

impl Dashboard {
    fn refresh(&self, w: &DashboardWidgets, sender: &ComponentSender<Self>) {
        sender.spawn_oneshot_command(|| DashboardCmd::Disks(list_disks()));
        for (index, item) in self.quick.iter().enumerate() {
            if let Some(q) = w.quick_rows.get(index) {
                q.spinner.set_visible(true);
                q.size.set_label("");
            }
            let path = item.path.clone();
            sender.spawn_oneshot_command(move || {
                let options = ScanOptions {
                    files_per_folder: 0,
                    largest_files: 0,
                    ..ScanOptions::default()
                };
                let result = scan(&path, &options, &ScanProgress::default())
                    .map(|r| (r.root.size, r.root.file_count, r.unreadable_folders > 0))
                    .map_err(|e| e.to_string());
                DashboardCmd::QuickSize { index, result }
            });
        }
    }
}

fn quick_items(system: &SystemInfo) -> Vec<QuickItem> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut items = Vec::new();

    if let Some(home) = &home {
        items.push(QuickItem {
            title: "App caches",
            description: "Temporary files apps can recreate",
            path: dirs_cache().unwrap_or_else(|| home.join(".cache")),
        });
        items.push(QuickItem {
            title: "Trash",
            description: "Deleted files that still take up space",
            path: home.join(".local/share/Trash"),
        });
    }

    let package_caches: &[(PackageManager, &str)] = &[
        (PackageManager::Pacman, "/var/cache/pacman/pkg"),
        (PackageManager::Apt, "/var/cache/apt/archives"),
        (PackageManager::Dnf, "/var/cache/libdnf5"),
        (PackageManager::Dnf, "/var/cache/dnf"),
        (PackageManager::Zypper, "/var/cache/zypp/packages"),
    ];
    if let Some((_, path)) = package_caches
        .iter()
        .find(|(pm, path)| system.has(*pm) && std::path::Path::new(path).is_dir())
    {
        items.push(QuickItem {
            title: "Package cache",
            description: "Downloaded installers of past updates",
            path: PathBuf::from(path),
        });
    }

    let journal = PathBuf::from("/var/log/journal");
    items.push(QuickItem {
        title: "System logs",
        description: "Logs from the system and apps",
        path: if journal.is_dir() {
            journal
        } else {
            PathBuf::from("/var/log")
        },
    });

    items.retain(|i| i.path.is_dir());
    items
}

fn dirs_cache() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

fn disk_row(disk: &Disk, sender: &ComponentSender<Dashboard>) -> adw::ActionRow {
    let fraction = disk.used_fraction();
    let row = {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .activatable(true)
            .build();
        row.set_title(&disk.label);
        row.set_subtitle(
            &(format!(
                "{} free of {} · {} · {}",
                human_size(disk.available),
                human_size(disk.total),
                disk.mount_point.display(),
                disk.fs_type
            )),
        );
        row
    };
    row.add_prefix(&gtk::Image::from_icon_name("drive-harddisk-symbolic"));

    let bar = gtk::LevelBar::builder()
        .value(fraction.clamp(0.0, 1.0))
        .width_request(120)
        .valign(gtk::Align::Center)
        .build();
    // Accent color, or the warning color when the disk is almost full.
    for offset in ["low", "high", "full"] {
        bar.remove_offset_value(Some(offset));
    }
    if fraction > ALMOST_FULL {
        bar.add_css_class("almost-full");
        bar.set_tooltip_text(Some("This disk is almost full"));
    }

    let used = gtk::Label::builder()
        .label(format!("{} used", percent(fraction)))
        .css_classes(["numeric", "caption"])
        .width_chars(8)
        .xalign(1.0)
        .build();
    let suffix = gtk::Box::builder()
        .spacing(12)
        .valign(gtk::Align::Center)
        .build();
    suffix.append(&bar);
    suffix.append(&used);
    row.add_suffix(&suffix);
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));

    let sender = sender.clone();
    let path = disk.mount_point.clone();
    row.connect_activated(move |_| {
        let _ = sender.output(DashboardOutput::Analyze(path.clone()));
    });
    row
}

fn system_group(system: &SystemInfo) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("This Computer")
        .build();
    group.add(&info_row("Distribution", &system.distro_name));
    group.add(&info_row("Kernel", or_unknown(&system.kernel)));
    group.add(&info_row(
        "Desktop",
        system.desktop.as_deref().unwrap_or("Unknown"),
    ));
    let managers = if system.package_managers.is_empty() {
        "None found".to_owned()
    } else {
        system
            .package_managers
            .iter()
            .map(|pm| pm.name())
            .collect::<Vec<_>>()
            .join(", ")
    };
    group.add(&info_row("Package managers", &managers));
    group
}

fn safety_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("How Cleaning Works")
        .build();
    for (icon, title, subtitle) in [
        (
            "view-reveal-symbolic",
            "You always see a preview",
            "Nothing is removed until you've reviewed the list and confirmed.",
        ),
        (
            "user-trash-symbolic",
            "Files go to the Trash",
            "You can restore them. Only caches are deleted directly.",
        ),
        (
            "security-high-symbolic",
            "System and private files are protected",
            "See the full list under Settings.",
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name(icon));
        group.add(&row);
    }
    group
}

fn info_row(title: &str, value: &str) -> adw::ActionRow {
    let row = {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        row.set_title(title);
        row.set_subtitle(value);
        row
    };
    row.add_css_class("property");
    row
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() { "Unknown" } else { s }
}
