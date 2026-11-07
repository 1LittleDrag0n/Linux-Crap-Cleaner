//! Storage Analyzer: scan a folder or disk, see what takes up space in a ring
//! chart and a sorted list, drill into folders, and remove what you don't
//! need (after confirming).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lcc_core::disks::{Disk, list_disks};
use lcc_core::format::{count, duration, human_size, percent};
use lcc_core::remove::remove;
use lcc_core::safety::PathGuard;
use lcc_core::scan::{EntryKind, ScanOptions, ScanProgress, ScanResult, scan};
use lcc_core::settings::{DeleteMode, Settings};
use relm4::adw::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

use crate::widgets::ring_chart::{RingChart, assign_slots, swatch};

/// Rows shown per folder; the rest are summarised in one row.
const MAX_ROWS: usize = 300;

enum State {
    Idle,
    Scanning {
        root: PathBuf,
        progress: Arc<ScanProgress>,
    },
    Done(ScanResult),
}

pub struct StoragePage {
    settings: Settings,
    state: State,
    /// Folder being shown, as names below the scan root.
    location: Vec<OsString>,
    disks: Vec<Disk>,
    tick: Option<glib::SourceId>,
}

#[derive(Debug)]
pub enum StorageMsg {
    Scan(PathBuf),
    Rescan,
    NewScan,
    Cancel,
    Tick,
    /// Open a folder, given as names below the shown folder.
    Open(Vec<OsString>),
    /// Go to the folder at this depth in the breadcrumbs (0 = scan root).
    GoTo(usize),
    Up,
    AskRemove {
        path: PathBuf,
        size: u64,
        files: u64,
        is_folder: bool,
    },
    Remove {
        path: PathBuf,
        size: u64,
    },
    ShowInFiles {
        path: PathBuf,
        is_folder: bool,
    },
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum StorageOutput {
    Toast(String),
}

#[derive(Debug)]
pub enum StorageCmd {
    Disks(Vec<Disk>),
    Finished {
        progress: Arc<ScanProgress>,
        result: Result<ScanResult, String>,
    },
    Removed {
        path: PathBuf,
        size: u64,
        mode: DeleteMode,
        result: Result<(), String>,
    },
}

pub struct StorageWidgets {
    start_list: gtk::ListBox,
    scanning_status: adw::StatusPage,
    breadcrumbs: gtk::Box,
    summary: gtk::Label,
    chart: RingChart,
    entry_list: gtk::ListBox,
    largest_list: gtk::ListBox,
}

impl Component for StoragePage {
    type Init = Settings;
    type Input = StorageMsg;
    type Output = StorageOutput;
    type CommandOutput = StorageCmd;
    type Root = gtk::Stack;
    type Widgets = StorageWidgets;

    fn init_root() -> Self::Root {
        gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vhomogeneous(false)
            .build()
    }

    fn init(
        settings: Self::Init,
        stack: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Start: pick what to scan.
        let start_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let start = adw::StatusPage::builder()
            .icon_name("drive-harddisk-symbolic")
            .title("Storage Analyzer")
            .description("See what's taking up space. Choose what to scan:")
            .child(
                &adw::Clamp::builder()
                    .maximum_size(560)
                    .child(&start_list)
                    .build(),
            )
            .build();
        stack.add_named(&scrolled(&start), Some("start"));

        // Scanning: spinner, live counts, cancel.
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        {
            let sender = sender.clone();
            cancel.connect_clicked(move |_| sender.input(StorageMsg::Cancel));
        }
        let spinner_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        spinner_box.append(
            &gtk::Spinner::builder()
                .spinning(true)
                .width_request(48)
                .height_request(48)
                .halign(gtk::Align::Center)
                .build(),
        );
        spinner_box.append(&cancel);
        let scanning_status = adw::StatusPage::builder()
            .title("Scanning…")
            .child(&spinner_box)
            .build();
        stack.add_named(&scanning_status, Some("scanning"));

        // Results.
        let breadcrumbs = gtk::Box::builder().spacing(2).build();
        let crumbs_scroller = gtk::ScrolledWindow::builder()
            .child(&breadcrumbs)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let rescan = icon_button("view-refresh-symbolic", "Scan again");
        {
            let sender = sender.clone();
            rescan.connect_clicked(move |_| sender.input(StorageMsg::Rescan));
        }
        let new_scan = icon_button("go-home-symbolic", "Scan something else");
        {
            let sender = sender.clone();
            new_scan.connect_clicked(move |_| sender.input(StorageMsg::NewScan));
        }
        let top_row = gtk::Box::builder()
            .spacing(6)
            .margin_top(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        top_row.append(&crumbs_scroller);
        top_row.append(&rescan);
        top_row.append(&new_scan);

        let summary = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();

        let view_stack = adw::ViewStack::new();
        let switcher = adw::ViewSwitcher::builder()
            .stack(&view_stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        let second_row = gtk::Box::builder()
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        second_row.append(&summary);
        second_row.append(&switcher);

        // Folders view: chart beside the list (stacked on narrow windows).
        let chart = {
            let open_sender = sender.clone();
            let up_sender = sender.clone();
            RingChart::new(
                move |path| open_sender.input(StorageMsg::Open(path)),
                move || up_sender.input(StorageMsg::Up),
            )
        };
        let entry_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(6)
            .margin_end(12)
            .build();
        let list_scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&entry_list)
            .hexpand(true)
            .vexpand(true)
            .build();
        let folders_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .homogeneous(true)
            .build();
        folders_box.append(chart.widget());
        folders_box.append(&list_scroller);

        let folders_bin = adw::BreakpointBin::builder()
            .width_request(320)
            .height_request(300)
            .child(&folders_box)
            .build();
        let narrow = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 760sp").expect("valid breakpoint"),
        );
        narrow.add_setter(
            &folders_box,
            "orientation",
            Some(&gtk::Orientation::Vertical.to_value()),
        );
        narrow.add_setter(&folders_box, "homogeneous", Some(&false.to_value()));
        narrow.add_setter(chart.widget(), "vexpand", Some(&false.to_value()));
        narrow.add_setter(chart.widget(), "height-request", Some(&260.to_value()));
        folders_bin.add_breakpoint(narrow);

        let folders_page = view_stack.add_titled(&folders_bin, Some("folders"), "Folders");
        folders_page.set_icon_name(Some("folder-symbolic"));

        // Largest files view.
        let largest_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        let files_page = view_stack.add_titled(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::Clamp::builder()
                        .maximum_size(900)
                        .child(&largest_list)
                        .build(),
                )
                .vexpand(true)
                .build(),
            Some("files"),
            "Largest Files",
        );
        files_page.set_icon_name(Some("text-x-generic-symbolic"));

        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        results.append(&top_row);
        results.append(&second_row);
        results.append(&view_stack);
        stack.add_named(&results, Some("results"));

        let model = StoragePage {
            settings,
            state: State::Idle,
            location: Vec::new(),
            disks: Vec::new(),
            tick: None,
        };
        let widgets = StorageWidgets {
            start_list,
            scanning_status,
            breadcrumbs,
            summary,
            chart,
            entry_list,
            largest_list,
        };
        model.show_start(&widgets, &stack, &sender);
        model.refresh_disks(&sender);
        ComponentParts { model, widgets }
    }

    fn update_with_view(
        &mut self,
        w: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        stack: &Self::Root,
    ) {
        match msg {
            StorageMsg::Scan(root) => self.start_scan(root, w, stack, &sender),
            StorageMsg::Rescan => {
                if let State::Done(r) = &self.state {
                    let root = r.root_path.clone();
                    let keep = std::mem::take(&mut self.location);
                    self.start_scan(root, w, stack, &sender);
                    self.location = keep;
                }
            }
            StorageMsg::NewScan => {
                self.state = State::Idle;
                self.location.clear();
                self.refresh_disks(&sender);
                self.show_start(w, stack, &sender);
            }
            StorageMsg::Cancel => {
                if let State::Scanning { progress, .. } = &self.state {
                    progress.cancel();
                }
                self.stop_tick();
                self.state = State::Idle;
                self.show_start(w, stack, &sender);
            }
            StorageMsg::Tick => {
                if let State::Scanning { root, progress } = &self.state {
                    w.scanning_status
                        .set_title(&format!("Scanning {}", display_root(root)));
                    w.scanning_status.set_description(Some(&format!(
                        "{} files · {} folders · {}",
                        count(progress.files()),
                        count(progress.folders()),
                        human_size(progress.bytes())
                    )));
                }
            }
            StorageMsg::Open(path) => {
                self.location.extend(path);
                self.show_results(w, &sender, false);
            }
            StorageMsg::GoTo(depth) => {
                self.location.truncate(depth);
                self.show_results(w, &sender, false);
            }
            StorageMsg::Up => {
                self.location.pop();
                self.show_results(w, &sender, false);
            }
            StorageMsg::AskRemove {
                path,
                size,
                files,
                is_folder,
            } => self.ask_remove(path, size, files, is_folder, stack, &sender),
            StorageMsg::Remove { path, size } => {
                let mode = self.settings.delete_mode;
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                sender.spawn_oneshot_command(move || {
                    let result = remove(&path, mode, &guard).map_err(|e| e.to_string());
                    StorageCmd::Removed {
                        path,
                        size,
                        mode,
                        result,
                    }
                });
            }
            StorageMsg::ShowInFiles { path, is_folder } => {
                show_in_files(&path, is_folder, stack, &sender)
            }
            StorageMsg::SettingsChanged(settings) => self.settings = settings,
        }
    }

    fn update_cmd_with_view(
        &mut self,
        w: &mut Self::Widgets,
        msg: Self::CommandOutput,
        sender: ComponentSender<Self>,
        stack: &Self::Root,
    ) {
        match msg {
            StorageCmd::Disks(disks) => {
                self.disks = disks;
                if matches!(self.state, State::Idle) {
                    self.show_start(w, stack, &sender);
                }
            }
            StorageCmd::Finished { progress, result } => {
                // Ignore results of a cancelled or replaced scan.
                let current = matches!(&self.state, State::Scanning { progress: p, .. } if Arc::ptr_eq(p, &progress));
                if !current {
                    return;
                }
                self.stop_tick();
                match result {
                    Ok(r) => {
                        // Keep the shown folder on a rescan if it still exists.
                        while !self.location.is_empty() && r.node_at(&self.location).is_none() {
                            self.location.pop();
                        }
                        self.state = State::Done(r);
                        stack.set_visible_child_name("results");
                        self.show_results(w, &sender, true);
                    }
                    Err(e) => {
                        self.state = State::Idle;
                        self.show_start(w, stack, &sender);
                        let _ = sender.output(StorageOutput::Toast(format!("Scan failed: {e}")));
                    }
                }
            }
            StorageCmd::Removed {
                path,
                size,
                mode,
                result,
            } => {
                let name = file_name(&path);
                let text = match result {
                    Ok(()) => {
                        if let State::Done(r) = &mut self.state {
                            r.remove_path(&path);
                        }
                        self.show_results(w, &sender, true);
                        match mode {
                            DeleteMode::MoveToTrash => format!("Moved “{name}” to the Trash"),
                            DeleteMode::Permanent => {
                                format!("Deleted “{name}”, freed {}", human_size(size))
                            }
                        }
                    }
                    Err(e) => e,
                };
                let _ = sender.output(StorageOutput::Toast(text));
            }
        }
    }
}

impl StoragePage {
    fn refresh_disks(&self, sender: &ComponentSender<Self>) {
        // statvfs can block on network mounts, so ask off the main thread.
        sender.spawn_oneshot_command(|| StorageCmd::Disks(list_disks()));
    }

    fn start_scan(
        &mut self,
        root: PathBuf,
        w: &StorageWidgets,
        stack: &gtk::Stack,
        sender: &ComponentSender<Self>,
    ) {
        if let State::Scanning { progress, .. } = &self.state {
            progress.cancel();
        }
        self.stop_tick();

        let progress = Arc::new(ScanProgress::default());
        let options = ScanOptions::from_settings(&self.settings);
        {
            let progress = progress.clone();
            let root = root.clone();
            sender.spawn_oneshot_command(move || {
                let result = scan(&root, &options, &progress).map_err(|e| e.to_string());
                StorageCmd::Finished { progress, result }
            });
        }

        let tick_sender = sender.clone();
        self.tick = Some(glib::timeout_add_local(
            Duration::from_millis(120),
            move || {
                tick_sender.input(StorageMsg::Tick);
                glib::ControlFlow::Continue
            },
        ));

        w.scanning_status
            .set_title(&format!("Scanning {}", display_root(&root)));
        w.scanning_status.set_description(Some("Starting…"));
        stack.set_visible_child_name("scanning");
        self.location.clear();
        self.state = State::Scanning { root, progress };
    }

    fn stop_tick(&mut self) {
        if let Some(id) = self.tick.take() {
            id.remove();
        }
    }

    fn show_start(&self, w: &StorageWidgets, stack: &gtk::Stack, sender: &ComponentSender<Self>) {
        w.start_list.remove_all();

        if let Some(home) = dirs_home() {
            w.start_list.append(&target_row(
                "user-home-symbolic",
                "Home folder",
                &home.display().to_string(),
                home,
                sender,
            ));
        }
        for disk in &self.disks {
            let subtitle = format!(
                "{} · {} used of {} ({})",
                disk.mount_point.display(),
                human_size(disk.used),
                human_size(disk.total),
                percent(disk.used_fraction())
            );
            w.start_list.append(&target_row(
                "drive-harddisk-symbolic",
                &format!("{} disk", disk.label),
                &subtitle,
                disk.mount_point.clone(),
                sender,
            ));
        }

        let choose = adw::ActionRow::builder()
            .title("Choose a folder…")
            .activatable(true)
            .build();
        choose.add_prefix(&gtk::Image::from_icon_name("folder-open-symbolic"));
        choose.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let sender = sender.clone();
            choose.connect_activated(move |row| pick_folder(row, sender.clone()));
        }
        w.start_list.append(&choose);

        stack.set_visible_child_name("start");
    }

    fn show_results(
        &self,
        w: &StorageWidgets,
        sender: &ComponentSender<Self>,
        rebuild_largest: bool,
    ) {
        let State::Done(result) = &self.state else {
            return;
        };
        let node = result.node_at(&self.location).unwrap_or(&result.root);
        let folder_path = result.path_at(&self.location);

        // Breadcrumbs.
        while let Some(child) = w.breadcrumbs.first_child() {
            w.breadcrumbs.remove(&child);
        }
        let mut crumbs = vec![display_root(&result.root_path)];
        crumbs.extend(
            self.location
                .iter()
                .map(|n| n.to_string_lossy().into_owned()),
        );
        let last = crumbs.len() - 1;
        for (depth, label) in crumbs.into_iter().enumerate() {
            if depth > 0 {
                w.breadcrumbs.append(
                    &gtk::Label::builder()
                        .label("›")
                        .css_classes(["dim-label"])
                        .build(),
                );
            }
            let text = gtk::Label::builder()
                .label(&label)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(24)
                .build();
            if depth == last {
                // The folder being shown: plain bold text, not a button.
                text.add_css_class("heading");
                text.set_margin_start(10);
                text.set_margin_end(10);
                w.breadcrumbs.append(&text);
            } else {
                let button = gtk::Button::builder()
                    .child(&text)
                    .css_classes(["flat", "breadcrumb"])
                    .tooltip_text("Go to this folder")
                    .build();
                let sender = sender.clone();
                button.connect_clicked(move |_| sender.input(StorageMsg::GoTo(depth)));
                w.breadcrumbs.append(&button);
            }
        }

        // Summary of the whole scan.
        let mut summary = format!(
            "{} in {} files and {} folders · scanned in {}",
            human_size(result.root.size),
            count(result.root.file_count),
            count(result.root.folder_count),
            duration(result.elapsed)
        );
        if result.unreadable_folders > 0 {
            summary.push_str(&format!(
                " · {} folders couldn't be read without administrator access",
                count(result.unreadable_folders)
            ));
        }
        w.summary.set_label(&summary);

        w.chart.set_node(node, !self.location.is_empty());

        // Folder contents.
        w.entry_list.remove_all();
        let entries = node.entries();
        let slots = assign_slots(&entries);
        let parent_size = node.size.max(1);
        if entries.is_empty() {
            w.entry_list.append(
                &adw::ActionRow::builder()
                    .title(if node.unreadable {
                        "This folder couldn't be read without administrator access"
                    } else {
                        "This folder is empty"
                    })
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        for (entry, slot) in entries.iter().zip(&slots).take(MAX_ROWS) {
            let path = folder_path.join(entry.name);
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.add_prefix(&swatch(*slot));

            let fraction = entry.size as f64 / parent_size as f64;
            match entry.kind {
                EntryKind::Folder => {
                    let child = entry.node.expect("folders carry their node");
                    row.set_title(&entry.name.to_string_lossy());
                    row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
                    let subtitle = if let Some(reason) = child.skipped {
                        reason.describe().to_owned()
                    } else if child.unreadable {
                        "Couldn't be read without administrator access".to_owned()
                    } else {
                        format!("{} files", count(child.file_count))
                    };
                    row.set_subtitle(&subtitle);
                    row.add_suffix(&size_box(entry.size, fraction));
                    if child.skipped.is_none() {
                        row.add_suffix(&show_button(&path, true, sender));
                        row.add_suffix(&remove_button(
                            &path,
                            entry.size,
                            child.file_count,
                            true,
                            sender,
                        ));
                        row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                        row.set_activatable(true);
                        let sender = sender.clone();
                        let name = entry.name.to_owned();
                        row.connect_activated(move |_| {
                            sender.input(StorageMsg::Open(vec![name.clone()]))
                        });
                    }
                }
                EntryKind::File => {
                    row.set_title(&entry.name.to_string_lossy());
                    row.add_prefix(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
                    row.add_suffix(&size_box(entry.size, fraction));
                    row.add_suffix(&show_button(&path, false, sender));
                    row.add_suffix(&remove_button(&path, entry.size, 1, false, sender));
                }
                EntryKind::OtherFiles => {
                    row.set_title(&format!("{} smaller files", count(entry.file_count)));
                    row.set_subtitle("Too small to list one by one");
                    row.add_prefix(&gtk::Image::from_icon_name("view-more-symbolic"));
                    row.add_suffix(&size_box(entry.size, fraction));
                }
            }
            w.entry_list.append(&row);
        }
        if entries.len() > MAX_ROWS {
            let rest = &entries[MAX_ROWS..];
            let size: u64 = rest.iter().map(|e| e.size).sum();
            w.entry_list.append(
                &adw::ActionRow::builder()
                    .title(format!(
                        "{} more items, {} in total",
                        count(rest.len() as u64),
                        human_size(size)
                    ))
                    .css_classes(["dim-label"])
                    .build(),
            );
        }

        if rebuild_largest {
            w.largest_list.remove_all();
            if result.largest_files.is_empty() {
                w.largest_list
                    .append(&adw::ActionRow::builder().title("No files found").build());
            }
            for file in &result.largest_files {
                let parent = file.path.parent().unwrap_or(Path::new("/"));
                let row = adw::ActionRow::builder()
                    .use_markup(false)
                    .title(file_name(&file.path))
                    .subtitle(parent.display().to_string())
                    .subtitle_lines(1)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("text-x-generic-symbolic"));
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(human_size(file.size))
                        .css_classes(["numeric"])
                        .build(),
                );
                row.add_suffix(&show_button(&file.path, false, sender));
                row.add_suffix(&remove_button(&file.path, file.size, 1, false, sender));
                w.largest_list.append(&row);
            }
        }
    }

    fn ask_remove(
        &self,
        path: PathBuf,
        size: u64,
        files: u64,
        is_folder: bool,
        parent: &gtk::Stack,
        sender: &ComponentSender<Self>,
    ) {
        let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
        if let Err(protected) = guard.check(&path) {
            let _ = sender.output(StorageOutput::Toast(format!(
                "“{}” is protected: {}",
                file_name(&path),
                protected.reason
            )));
            return;
        }

        let name = file_name(&path);
        let what = if is_folder {
            format!("{} with {} files", human_size(size), count(files))
        } else {
            human_size(size)
        };
        let (heading, body, action) = match self.settings.delete_mode {
            DeleteMode::MoveToTrash => (
                format!("Move “{name}” to the Trash?"),
                format!(
                    "{what}\n{}\n\nYou can restore it from the Trash. Space is freed when you empty the Trash.",
                    path.display()
                ),
                "Move to Trash",
            ),
            DeleteMode::Permanent => (
                format!("Delete “{name}” permanently?"),
                format!("{what}\n{}\n\nThis can't be undone.", path.display()),
                "Delete",
            ),
        };

        let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", action);
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let sender = sender.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "remove" {
                sender.input(StorageMsg::Remove {
                    path: path.clone(),
                    size,
                });
            }
        });
        dialog.present(Some(parent));
    }
}

fn scrolled(child: &impl IsA<gtk::Widget>) -> gtk::ScrolledWindow {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(child)
        .build()
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build()
}

fn target_row(
    icon: &str,
    title: &str,
    subtitle: &str,
    path: PathBuf,
    sender: &ComponentSender<StoragePage>,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .title(title)
        .subtitle(subtitle)
        .activatable(true)
        .build();
    row.add_prefix(&gtk::Image::from_icon_name(icon));
    row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    let sender = sender.clone();
    row.connect_activated(move |_| sender.input(StorageMsg::Scan(path.clone())));
    row
}

/// Size label with a small bar showing the share of the parent folder.
fn size_box(size: u64, fraction: f64) -> gtk::Box {
    let b = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .valign(gtk::Align::Center)
        .width_request(72)
        .build();
    b.append(
        &gtk::Label::builder()
            .label(human_size(size))
            .xalign(1.0)
            .css_classes(["numeric"])
            .build(),
    );
    let bar = gtk::LevelBar::builder()
        .value(fraction.clamp(0.0, 1.0))
        .tooltip_text(format!("{} of this folder", percent(fraction)))
        .build();
    // Plain accent bar: no low/high/full warning colors.
    for offset in ["low", "high", "full"] {
        bar.remove_offset_value(Some(offset));
    }
    b.append(&bar);
    b
}

fn show_button(path: &Path, is_folder: bool, sender: &ComponentSender<StoragePage>) -> gtk::Button {
    let button = icon_button(
        "folder-open-symbolic",
        if is_folder {
            "Open in file manager"
        } else {
            "Show in file manager"
        },
    );
    let sender = sender.clone();
    let path = path.to_path_buf();
    button.connect_clicked(move |_| {
        sender.input(StorageMsg::ShowInFiles {
            path: path.clone(),
            is_folder,
        })
    });
    button
}

fn remove_button(
    path: &Path,
    size: u64,
    files: u64,
    is_folder: bool,
    sender: &ComponentSender<StoragePage>,
) -> gtk::Button {
    let button = icon_button("user-trash-symbolic", "Remove…");
    let sender = sender.clone();
    let path = path.to_path_buf();
    button.connect_clicked(move |_| {
        sender.input(StorageMsg::AskRemove {
            path: path.clone(),
            size,
            files,
            is_folder,
        })
    });
    button
}

fn show_in_files(
    path: &Path,
    is_folder: bool,
    parent: &gtk::Stack,
    sender: &ComponentSender<StoragePage>,
) {
    let window = parent.root().and_downcast::<gtk::Window>();
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let sender = sender.clone();
    let done = move |result: Result<(), glib::Error>| {
        if let Err(e) = result
            && !e.matches(gtk::DialogError::Dismissed)
        {
            let _ = sender.output(StorageOutput::Toast(format!(
                "Couldn't open the file manager: {e}"
            )));
        }
    };
    if is_folder {
        launcher.launch(window.as_ref(), None::<&gio::Cancellable>, done);
    } else {
        launcher.open_containing_folder(window.as_ref(), None::<&gio::Cancellable>, done);
    }
}

fn pick_folder(row: &adw::ActionRow, sender: ComponentSender<StoragePage>) {
    let window = row.root().and_downcast::<gtk::Window>();
    let dialog = gtk::FileDialog::builder()
        .title("Choose a Folder to Scan")
        .modal(true)
        .build();
    dialog.select_folder(window.as_ref(), None::<&gio::Cancellable>, move |result| {
        if let Ok(folder) = result {
            match folder.path() {
                Some(path) => sender.input(StorageMsg::Scan(path)),
                None => {
                    let _ = sender.output(StorageOutput::Toast(
                        "That folder isn't on this computer, so it can't be scanned.".to_owned(),
                    ));
                }
            }
        }
    });
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn display_root(root: &Path) -> String {
    match dirs_home() {
        Some(home) if home == root => "Home".to_owned(),
        _ => root.display().to_string(),
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}
