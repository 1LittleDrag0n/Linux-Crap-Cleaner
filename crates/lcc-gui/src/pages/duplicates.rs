//! Duplicates & Big Files: find identical files, very large files and empty
//! folders in chosen folders, select what to remove (by hand or with a
//! keep-one rule), confirm, remove.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lcc_core::duplicates::{
    FindOptions, FindProgress, FindResult, KeepRule, Stage, copies_to_remove, find, group_name,
};
use lcc_core::format::{ago, count, human_size, now, percent};
use lcc_core::remove::remove;
use lcc_core::safety::PathGuard;
use lcc_core::settings::{DeleteMode, Settings};
use relm4::adw::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

const MIN_SIZES: [(u64, &str); 4] = [
    (10_000, "10 kB"),
    (100_000, "100 kB"),
    (1_000_000, "1 MB"),
    (10_000_000, "10 MB"),
];

const RULES: [(KeepRule, &str); 3] = [
    (KeepRule::Newest, "Keep the newest copy"),
    (KeepRule::Oldest, "Keep the oldest copy"),
    (
        KeepRule::ShortestPath,
        "Keep the copy with the shortest path",
    ),
];

/// Duplicate groups shown; the rest are summarised.
const MAX_GROUPS: usize = 300;
/// Groups expanded when results appear.
const EXPANDED_GROUPS: usize = 10;

enum State {
    Setup,
    Searching(Arc<FindProgress>),
    Results(FindResult),
}

pub struct DuplicatesPage {
    settings: Settings,
    folders: Vec<PathBuf>,
    min_size: u64,
    include_hidden: bool,
    rule: KeepRule,
    state: State,
    selected: HashSet<PathBuf>,
    tick: Option<glib::SourceId>,
}

#[derive(Debug)]
pub enum DupMsg {
    AddFolder(PathBuf),
    RemoveFolder(PathBuf),
    MinSize(u32),
    IncludeHidden(bool),
    Find,
    Cancel,
    Tick,
    NewSearch,
    Rule(u32),
    SelectCopies,
    SelectAllEmpty,
    ClearSelection,
    Toggle(PathBuf, bool),
    AskRemove,
    Remove,
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum DupOutput {
    Toast(String),
}

#[derive(Debug)]
pub enum DupCmd {
    Found {
        progress: Arc<FindProgress>,
        result: Result<FindResult, String>,
    },
    Removed {
        removed: Vec<PathBuf>,
        failures: Vec<String>,
    },
}

pub struct DupWidgets {
    folders_group: adw::PreferencesGroup,
    folder_rows: Vec<adw::ActionRow>,
    progress_status: adw::StatusPage,
    progress_bar: gtk::ProgressBar,
    summary: gtk::Label,
    remove_button: gtk::Button,
    dup_page: adw::PreferencesPage,
    big_page: adw::PreferencesPage,
    empty_page: adw::PreferencesPage,
    result_groups: Vec<(adw::PreferencesPage, adw::PreferencesGroup)>,
    dup_view: adw::ViewStackPage,
    big_view: adw::ViewStackPage,
    empty_view: adw::ViewStackPage,
}

impl Component for DuplicatesPage {
    type Init = Settings;
    type Input = DupMsg;
    type Output = DupOutput;
    type CommandOutput = DupCmd;
    type Root = gtk::Stack;
    type Widgets = DupWidgets;

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
        // Setup
        let setup = adw::PreferencesPage::builder()
            .title("Duplicates & Big Files")
            .description("Find identical files, very large files and empty folders. Files are compared by their content, not just their names.")
            .build();
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Add a folder")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            add.connect_clicked(move |b| pick_folder(b, sender.clone()));
        }
        let folders_group = adw::PreferencesGroup::builder()
            .title("Folders to Search")
            .header_suffix(&add)
            .build();
        setup.add(&folders_group);

        let options = adw::PreferencesGroup::builder().title("Options").build();
        let min_size = adw::ComboRow::builder()
            .title("Ignore files smaller than")
            .model(&gtk::StringList::new(&MIN_SIZES.map(|(_, l)| l)))
            .selected(1)
            .build();
        {
            let sender = sender.clone();
            min_size.connect_selected_notify(move |r| sender.input(DupMsg::MinSize(r.selected())));
        }
        let hidden = adw::SwitchRow::builder()
            .title("Include hidden files and folders")
            .subtitle("App data, such as games, often holds copies on purpose. Leave this off unless you know what you're looking for.")
            .build();
        {
            let sender = sender.clone();
            hidden
                .connect_active_notify(move |r| sender.input(DupMsg::IncludeHidden(r.is_active())));
        }
        options.add(&min_size);
        options.add(&hidden);
        setup.add(&options);

        let find_button = gtk::Button::builder()
            .label("Find Duplicates")
            .halign(gtk::Align::Center)
            .margin_top(12)
            .css_classes(["pill", "suggested-action"])
            .build();
        {
            let sender = sender.clone();
            find_button.connect_clicked(move |_| sender.input(DupMsg::Find));
        }
        let button_group = adw::PreferencesGroup::new();
        button_group.add(&find_button);
        setup.add(&button_group);
        stack.add_named(&setup, Some("setup"));

        // Searching
        let progress_bar = gtk::ProgressBar::builder().width_request(280).build();
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        {
            let sender = sender.clone();
            cancel.connect_clicked(move |_| sender.input(DupMsg::Cancel));
        }
        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .halign(gtk::Align::Center)
            .build();
        progress_box.append(&progress_bar);
        progress_box.append(&cancel);
        let progress_status = adw::StatusPage::builder()
            .icon_name("edit-copy-symbolic")
            .title("Looking at files…")
            .child(&progress_box)
            .build();
        stack.add_named(&progress_status, Some("searching"));

        // Results
        let summary = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let new_search = gtk::Button::builder()
            .icon_name("edit-find-symbolic")
            .tooltip_text("New search")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            new_search.connect_clicked(move |_| sender.input(DupMsg::NewSearch));
        }
        let remove_button = gtk::Button::builder()
            .label("Remove…")
            .valign(gtk::Align::Center)
            .css_classes(["pill", "destructive-action"])
            .build();
        {
            let sender = sender.clone();
            remove_button.connect_clicked(move |_| sender.input(DupMsg::AskRemove));
        }
        let header = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(18)
            .margin_end(18)
            .build();
        header.append(&summary);
        header.append(&new_search);
        header.append(&remove_button);

        let view_stack = adw::ViewStack::builder().vexpand(true).build();
        let dup_page = adw::PreferencesPage::new();
        let big_page = adw::PreferencesPage::new();
        let empty_page = adw::PreferencesPage::new();
        let dup_view = view_stack.add_titled_with_icon(
            &dup_page,
            Some("duplicates"),
            "Duplicates",
            "edit-copy-symbolic",
        );
        let big_view = view_stack.add_titled_with_icon(
            &big_page,
            Some("big"),
            "Big Files",
            "drive-harddisk-symbolic",
        );
        let empty_view = view_stack.add_titled_with_icon(
            &empty_page,
            Some("empty"),
            "Empty Folders",
            "folder-symbolic",
        );
        let switcher = adw::ViewSwitcher::builder()
            .stack(&view_stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .halign(gtk::Align::Center)
            .margin_bottom(6)
            .build();

        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        results.append(
            &adw::Clamp::builder()
                .maximum_size(800)
                .child(&header)
                .build(),
        );
        results.append(&switcher);
        results.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        results.append(&view_stack);
        stack.add_named(&results, Some("results"));

        stack.add_named(
            &adw::StatusPage::builder()
                .title("Removing…")
                .child(
                    &gtk::Spinner::builder()
                        .spinning(true)
                        .width_request(48)
                        .height_request(48)
                        .halign(gtk::Align::Center)
                        .build(),
                )
                .build(),
            Some("removing"),
        );

        let model = DuplicatesPage {
            settings,
            folders: std::env::var_os("HOME")
                .map(PathBuf::from)
                .into_iter()
                .collect(),
            min_size: MIN_SIZES[1].0,
            include_hidden: false,
            rule: KeepRule::Newest,
            state: State::Setup,
            selected: HashSet::new(),
            tick: None,
        };
        let mut widgets = DupWidgets {
            folders_group,
            folder_rows: Vec::new(),
            progress_status,
            progress_bar,
            summary,
            remove_button,
            dup_page,
            big_page,
            empty_page,
            result_groups: Vec::new(),
            dup_view,
            big_view,
            empty_view,
        };
        model.show_folders(&mut widgets, &sender);
        stack.set_visible_child_name("setup");
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
            DupMsg::AddFolder(path) => {
                if !self.folders.contains(&path) {
                    self.folders.push(path);
                    self.show_folders(w, &sender);
                }
            }
            DupMsg::RemoveFolder(path) => {
                self.folders.retain(|f| *f != path);
                self.show_folders(w, &sender);
            }
            DupMsg::MinSize(i) => {
                if let Some((size, _)) = MIN_SIZES.get(i as usize) {
                    self.min_size = *size;
                }
            }
            DupMsg::IncludeHidden(on) => self.include_hidden = on,
            DupMsg::Find => self.start(w, stack, &sender),
            DupMsg::Cancel => {
                if let State::Searching(p) = &self.state {
                    p.cancel();
                }
                self.stop_tick();
                self.state = State::Setup;
                stack.set_visible_child_name("setup");
            }
            DupMsg::Tick => {
                if let State::Searching(p) = &self.state {
                    match p.stage() {
                        Stage::Listing => {
                            w.progress_status.set_title("Looking at files…");
                            w.progress_status.set_description(Some(&format!(
                                "{} files found",
                                count(p.files())
                            )));
                            w.progress_bar.pulse();
                        }
                        Stage::Comparing => {
                            let total = p.bytes_to_compare().max(1);
                            let fraction = p.bytes_compared() as f64 / total as f64;
                            w.progress_status
                                .set_title("Comparing files with the same size…");
                            w.progress_status.set_description(Some(&format!(
                                "{} of {} read · {}",
                                human_size(p.bytes_compared()),
                                human_size(p.bytes_to_compare()),
                                percent(fraction)
                            )));
                            w.progress_bar.set_fraction(fraction.min(1.0));
                        }
                    }
                }
            }
            DupMsg::NewSearch => {
                self.state = State::Setup;
                self.selected.clear();
                stack.set_visible_child_name("setup");
            }
            DupMsg::Rule(i) => {
                if let Some((rule, _)) = RULES.get(i as usize) {
                    self.rule = *rule;
                }
            }
            DupMsg::SelectCopies => {
                if let State::Results(r) = &self.state {
                    for g in &r.duplicates {
                        for f in &g.files {
                            self.selected.remove(&f.path);
                        }
                        self.selected.extend(
                            copies_to_remove(g, self.rule)
                                .into_iter()
                                .map(Path::to_path_buf),
                        );
                    }
                }
                self.show_results(w, &sender);
            }
            DupMsg::SelectAllEmpty => {
                if let State::Results(r) = &self.state {
                    self.selected.extend(r.empty_folders.iter().cloned());
                }
                self.show_results(w, &sender);
            }
            DupMsg::ClearSelection => {
                self.selected.clear();
                self.show_results(w, &sender);
            }
            DupMsg::Toggle(path, on) => {
                if on {
                    self.selected.insert(path);
                } else {
                    self.selected.remove(&path);
                }
                self.update_summary(w);
            }
            DupMsg::AskRemove => self.ask_remove(stack, &sender),
            DupMsg::Remove => {
                let paths: Vec<PathBuf> = self.selected.iter().cloned().collect();
                if paths.is_empty() {
                    return;
                }
                if !matches!(self.state, State::Results(_)) {
                    return;
                }
                stack.set_visible_child_name("removing");
                let mode = self.settings.delete_mode;
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                sender.spawn_oneshot_command(move || {
                    let mut removed = Vec::new();
                    let mut failures = Vec::new();
                    for p in paths {
                        match remove(&p, mode, &guard) {
                            Ok(()) => removed.push(p),
                            Err(e) => failures.push(e.to_string()),
                        }
                    }
                    DupCmd::Removed { removed, failures }
                });
            }
            DupMsg::SettingsChanged(s) => self.settings = s,
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
            DupCmd::Found { progress, result } => {
                let current =
                    matches!(&self.state, State::Searching(p) if Arc::ptr_eq(p, &progress));
                if !current {
                    return;
                }
                self.stop_tick();
                match result {
                    Ok(r) => {
                        self.state = State::Results(r);
                        self.selected.clear();
                        self.show_results(w, &sender);
                        stack.set_visible_child_name("results");
                    }
                    Err(e) => {
                        self.state = State::Setup;
                        stack.set_visible_child_name("setup");
                        let _ = sender.output(DupOutput::Toast(format!("Search failed: {e}")));
                    }
                }
            }
            DupCmd::Removed { removed, failures } => {
                if let State::Results(r) = &mut self.state {
                    for p in &removed {
                        r.forget(p);
                    }
                }
                for p in &removed {
                    self.selected.remove(p);
                }
                let mut text = format!(
                    "Removed {} {}",
                    count(removed.len() as u64),
                    if removed.len() == 1 { "item" } else { "items" }
                );
                if let Some(first) = failures.first() {
                    text.push_str(&format!(
                        ". {} failed: {first}",
                        count(failures.len() as u64)
                    ));
                }
                let _ = sender.output(DupOutput::Toast(text));
                self.show_results(w, &sender);
                stack.set_visible_child_name("results");
            }
        }
    }
}

impl DuplicatesPage {
    fn start(&mut self, w: &DupWidgets, stack: &gtk::Stack, sender: &ComponentSender<Self>) {
        if self.folders.is_empty() {
            let _ = sender.output(DupOutput::Toast("Add a folder to search first".to_owned()));
            return;
        }
        let progress = Arc::new(FindProgress::default());
        let options = FindOptions {
            roots: self.folders.clone(),
            min_size: self.min_size,
            include_hidden: self.include_hidden,
            excluded: self.settings.excluded_paths.clone(),
            stay_on_one_filesystem: self.settings.stay_on_one_filesystem,
            ..FindOptions::default()
        };
        let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
        {
            let progress = progress.clone();
            sender.spawn_oneshot_command(move || {
                let result = find(&options, &guard, &progress).map_err(|e| e.to_string());
                DupCmd::Found { progress, result }
            });
        }
        let tick_sender = sender.clone();
        self.tick = Some(glib::timeout_add_local(
            Duration::from_millis(120),
            move || {
                tick_sender.input(DupMsg::Tick);
                glib::ControlFlow::Continue
            },
        ));
        w.progress_bar.set_fraction(0.0);
        w.progress_status.set_title("Looking at files…");
        w.progress_status.set_description(Some("Starting…"));
        self.state = State::Searching(progress);
        stack.set_visible_child_name("searching");
    }

    fn stop_tick(&mut self) {
        if let Some(id) = self.tick.take() {
            id.remove();
        }
    }

    fn show_folders(&self, w: &mut DupWidgets, sender: &ComponentSender<Self>) {
        for row in w.folder_rows.drain(..) {
            w.folders_group.remove(&row);
        }
        for folder in &self.folders {
            let row = {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&folder.display().to_string());
                row
            };
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .tooltip_text("Don't search this folder")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let sender = sender.clone();
            let path = folder.clone();
            remove.connect_clicked(move |_| sender.input(DupMsg::RemoveFolder(path.clone())));
            row.add_suffix(&remove);
            w.folders_group.add(&row);
            w.folder_rows.push(row);
        }
        if self.folders.is_empty() {
            let row = adw::ActionRow::builder()
                .title("No folders yet. Add one with the + button.")
                .css_classes(["dim-label"])
                .build();
            w.folders_group.add(&row);
            w.folder_rows.push(row);
        }
    }

    fn result(&self) -> Option<&FindResult> {
        match &self.state {
            State::Results(r) => Some(r),
            _ => None,
        }
    }

    /// Size of a selected path, for totals.
    fn size_of(&self, path: &Path) -> u64 {
        let Some(r) = self.result() else { return 0 };
        r.duplicates
            .iter()
            .flat_map(|g| &g.files)
            .chain(&r.big_files)
            .find(|f| f.path == path)
            .map(|f| f.size)
            .unwrap_or(0)
    }

    fn update_summary(&self, w: &DupWidgets) {
        let Some(r) = self.result() else { return };
        let wasted: u64 = r.duplicates.iter().map(|g| g.wasted()).sum();
        let selected_size: u64 = self.selected.iter().map(|p| self.size_of(p)).sum();
        let found = format!(
            "{} files checked in {} · {} can be freed by removing copies",
            count(r.files_checked),
            lcc_core::format::duration(r.elapsed),
            human_size(wasted)
        );
        w.summary.set_label(&if self.selected.is_empty() {
            format!("{found}\nNothing selected")
        } else {
            format!(
                "{found}\n{} selected, {}",
                count(self.selected.len() as u64),
                human_size(selected_size)
            )
        });
        w.remove_button.set_sensitive(!self.selected.is_empty());
    }

    fn show_results(&self, w: &mut DupWidgets, sender: &ComponentSender<Self>) {
        for (page, group) in w.result_groups.drain(..) {
            page.remove(&group);
        }
        let Some(r) = self.result() else { return };
        let now = now();
        let home = std::env::var_os("HOME").map(PathBuf::from);

        // Duplicates
        w.dup_view.set_badge_number(r.duplicates.len() as u32);
        let wasted: u64 = r.duplicates.iter().map(|g| g.wasted()).sum();
        let tools = adw::PreferencesGroup::builder()
            .title("Identical Files")
            .description(if r.duplicates.is_empty() {
                "No duplicates found.".to_owned()
            } else {
                format!(
                    "{} groups of identical files · {} can be freed. Copies inside software projects are never selected automatically.",
                    count(r.duplicates.len() as u64),
                    human_size(wasted)
                )
            })
            .build();
        if !r.duplicates.is_empty() {
            let rule = adw::ComboRow::builder()
                .title("Automatic selection")
                .model(&gtk::StringList::new(&RULES.map(|(_, l)| l)))
                .selected(RULES.iter().position(|(k, _)| *k == self.rule).unwrap_or(0) as u32)
                .build();
            {
                let sender = sender.clone();
                rule.connect_selected_notify(move |r| sender.input(DupMsg::Rule(r.selected())));
            }
            let select = button("Select Copies", "suggested-action");
            {
                let sender = sender.clone();
                select.connect_clicked(move |_| sender.input(DupMsg::SelectCopies));
            }
            let clear = button("Clear", "flat");
            {
                let sender = sender.clone();
                clear.connect_clicked(move |_| sender.input(DupMsg::ClearSelection));
            }
            rule.add_suffix(&select);
            rule.add_suffix(&clear);
            tools.add(&rule);
        }
        w.dup_page.add(&tools);
        w.result_groups.push((w.dup_page.clone(), tools));

        if !r.duplicates.is_empty() {
            let list = adw::PreferencesGroup::new();
            for (i, g) in r.duplicates.iter().take(MAX_GROUPS).enumerate() {
                let all_selected = g.files.iter().all(|f| self.selected.contains(&f.path));
                let mut subtitle = format!(
                    "{} copies × {} · {} can be freed",
                    g.files.len(),
                    human_size(g.size),
                    human_size(g.wasted())
                );
                if all_selected {
                    subtitle.push_str(" · every copy is selected");
                }
                let row = {
                    let row = adw::ExpanderRow::builder()
                        .use_markup(false)
                        .expanded(i < EXPANDED_GROUPS)
                        .build();
                    row.set_title(&group_name(g));
                    row.set_subtitle(&subtitle);
                    row
                };
                row.add_prefix(&gtk::Image::from_icon_name("edit-copy-symbolic"));
                for f in &g.files {
                    let mut detail = format!("changed {}", ago(f.modified, now));
                    if f.in_project {
                        detail.push_str(" · in a software project");
                    }
                    let parent = f.path.parent().unwrap_or(Path::new("/"));
                    row.add_row(&self.file_row(
                        &short(parent, home.as_deref()),
                        &detail,
                        &f.path,
                        None,
                        sender,
                    ));
                }
                list.add(&row);
            }
            if r.duplicates.len() > MAX_GROUPS {
                list.add(
                    &adw::ActionRow::builder()
                        .title(format!(
                            "{} smaller groups not shown",
                            count((r.duplicates.len() - MAX_GROUPS) as u64)
                        ))
                        .css_classes(["dim-label"])
                        .build(),
                );
            }
            w.dup_page.add(&list);
            w.result_groups.push((w.dup_page.clone(), list));
        }

        // Big files
        w.big_view.set_badge_number(r.big_files.len() as u32);
        let big = adw::PreferencesGroup::builder()
            .title("Big Files")
            .description(if r.big_files.is_empty() {
                "No files over 50 MB.".to_owned()
            } else {
                "Files over 50 MB, largest first.".to_owned()
            })
            .build();
        for f in &r.big_files {
            let name = f
                .path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            let parent = f.path.parent().unwrap_or(Path::new("/"));
            let detail = format!(
                "{} · changed {}",
                short(parent, home.as_deref()),
                ago(f.modified, now)
            );
            big.add(&self.file_row(&name, &detail, &f.path, Some(f.size), sender));
        }
        w.big_page.add(&big);
        w.result_groups.push((w.big_page.clone(), big));

        // Empty folders
        w.empty_view.set_badge_number(r.empty_folders.len() as u32);
        let select_all = button("Select All", "flat");
        {
            let sender = sender.clone();
            select_all.connect_clicked(move |_| sender.input(DupMsg::SelectAllEmpty));
        }
        select_all.set_visible(!r.empty_folders.is_empty());
        let empty = adw::PreferencesGroup::builder()
            .title("Empty Folders")
            .description(if r.empty_folders.is_empty() {
                "No empty folders found.".to_owned()
            } else {
                "Folders with nothing inside. Folders in software projects and hidden folders aren't listed.".to_owned()
            })
            .header_suffix(&select_all)
            .build();
        for p in &r.empty_folders {
            empty.add(&self.file_row(&short(p, home.as_deref()), "Empty", p, None, sender));
        }
        w.empty_page.add(&empty);
        w.result_groups.push((w.empty_page.clone(), empty));

        self.update_summary(w);
    }

    fn file_row(
        &self,
        title: &str,
        subtitle: &str,
        path: &Path,
        size: Option<u64>,
        sender: &ComponentSender<Self>,
    ) -> adw::ActionRow {
        let row = {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(title);
            row.set_subtitle(subtitle);
            row
        };
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(self.selected.contains(path))
            .build();
        check.update_property(&[gtk::accessible::Property::Label(&format!(
            "Select {}",
            path.display()
        ))]);
        {
            let sender = sender.clone();
            let path = path.to_path_buf();
            check.connect_toggled(move |c| {
                sender.input(DupMsg::Toggle(path.clone(), c.is_active()))
            });
        }
        row.add_prefix(&check);
        row.set_activatable_widget(Some(&check));
        if let Some(size) = size {
            row.add_suffix(
                &gtk::Label::builder()
                    .label(human_size(size))
                    .css_classes(["numeric"])
                    .build(),
            );
        }
        let show = gtk::Button::builder()
            .icon_name("folder-open-symbolic")
            .tooltip_text("Show in file manager")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let path = path.to_path_buf();
            let sender = sender.clone();
            show.connect_clicked(move |b| show_in_files(b, &path, &sender));
        }
        row.add_suffix(&show);
        row
    }

    fn ask_remove(&self, parent: &gtk::Stack, sender: &ComponentSender<Self>) {
        let Some(r) = self.result() else { return };
        if self.selected.is_empty() {
            return;
        }
        // Never remove every copy of a file.
        let all_gone: Vec<String> = r
            .duplicates
            .iter()
            .filter(|g| g.files.iter().all(|f| self.selected.contains(&f.path)))
            .map(group_name)
            .collect();
        if !all_gone.is_empty() {
            let shown: Vec<&str> = all_gone.iter().take(5).map(String::as_str).collect();
            let dialog = adw::AlertDialog::new(
                Some("Keep at least one copy"),
                Some(&format!(
                    "Every copy is selected for: {}{}. Untick one copy of each before removing.",
                    shown.join(", "),
                    if all_gone.len() > 5 {
                        format!(" and {} more", all_gone.len() - 5)
                    } else {
                        String::new()
                    }
                )),
            );
            dialog.add_response("ok", "OK");
            dialog.present(Some(parent));
            return;
        }

        let n = self.selected.len();
        let size: u64 = self.selected.iter().map(|p| self.size_of(p)).sum();
        let how = match self.settings.delete_mode {
            DeleteMode::MoveToTrash => {
                "They'll be moved to the Trash, where you can restore them. Space is freed when you empty the Trash."
            }
            DeleteMode::Permanent => "They'll be deleted permanently. This can't be undone.",
        };
        let dialog = adw::AlertDialog::new(
            Some(&format!(
                "Remove {} {}?",
                count(n as u64),
                if n == 1 { "item" } else { "items" }
            )),
            Some(&format!("{}\n\n{how}", human_size(size))),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let sender = sender.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp == "remove" {
                sender.input(DupMsg::Remove);
            }
        });
        dialog.present(Some(parent));
    }
}

fn button(label: &str, class: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .css_classes([class])
        .build()
}

fn short(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn pick_folder(button: &gtk::Button, sender: ComponentSender<DuplicatesPage>) {
    let window = button.root().and_downcast::<gtk::Window>();
    let dialog = gtk::FileDialog::builder()
        .title("Choose a Folder to Search")
        .modal(true)
        .build();
    dialog.select_folder(window.as_ref(), None::<&gio::Cancellable>, move |result| {
        if let Ok(folder) = result
            && let Some(path) = folder.path()
        {
            sender.input(DupMsg::AddFolder(path));
        }
    });
}

fn show_in_files(button: &gtk::Button, path: &Path, sender: &ComponentSender<DuplicatesPage>) {
    let window = button.root().and_downcast::<gtk::Window>();
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let sender = sender.clone();
    launcher.open_containing_folder(window.as_ref(), None::<&gio::Cancellable>, move |r| {
        if let Err(e) = r
            && !e.matches(gtk::DialogError::Dismissed)
        {
            let _ = sender.output(DupOutput::Toast(format!(
                "Couldn't open the file manager: {e}"
            )));
        }
    });
}
