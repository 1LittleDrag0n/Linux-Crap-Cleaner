//! Junk Cleaner: scan for caches and leftovers, review them with sizes, pick
//! what to remove, confirm, clean.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use lcc_core::format::{count, human_size};
use lcc_core::junk::{
    AnalyzeOptions, Category, CleanProgress, CleanReport, Cleaner, Finding, Places, Status,
    analyze, builtin_cleaners, clean, root_operation, running_processes,
};
use lcc_core::privileged::{Operation, run_privileged};
use lcc_core::safety::PathGuard;
use lcc_core::settings::Settings;
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

/// Items listed inside each expanded row.
const MAX_ITEMS: usize = 30;

enum State {
    Intro,
    Analyzing,
    Results(Vec<Finding>),
    Cleaning(Arc<CleanProgress>),
    Done,
}

pub struct JunkPage {
    settings: Settings,
    cleaners: Vec<Cleaner>,
    state: State,
    /// User's checkbox choices this session, by cleaner id.
    choices: HashMap<String, bool>,
    tick: Option<glib::SourceId>,
}

#[derive(Debug)]
pub enum JunkMsg {
    Analyze,
    Toggle(String, bool),
    AskClean,
    Clean,
    Tick,
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum JunkOutput {
    /// Files were removed; other screens may want to refresh.
    Cleaned,
}

#[derive(Debug)]
pub enum JunkCmd {
    Analyzed(Vec<Finding>),
    Cleaned(CleanReport),
}

pub struct JunkWidgets {
    total: gtk::Label,
    total_caption: gtk::Label,
    clean_button: gtk::Button,
    results_page: adw::PreferencesPage,
    groups: Vec<adw::PreferencesGroup>,
    progress_bar: gtk::ProgressBar,
    progress_label: gtk::Label,
    done_status: adw::StatusPage,
    done_details: gtk::ListBox,
}

impl Component for JunkPage {
    type Init = Settings;
    type Input = JunkMsg;
    type Output = JunkOutput;
    type CommandOutput = JunkCmd;
    type Root = gtk::Stack;
    type Widgets = JunkWidgets;

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
        // Intro
        let scan_button = pill("Scan for Junk", true);
        {
            let sender = sender.clone();
            scan_button.connect_clicked(move |_| sender.input(JunkMsg::Analyze));
        }
        let intro = adw::StatusPage::builder()
            .icon_name("user-trash-symbolic")
            .title("Junk Cleaner")
            .description(
                "Find caches, leftovers and logs that are safe to remove.\nNothing is deleted until you've reviewed the list and confirmed.",
            )
            .child(&scan_button)
            .build();
        stack.add_named(&intro, Some("intro"));

        // Analyzing
        stack.add_named(
            &adw::StatusPage::builder()
                .title("Looking for junk…")
                .description("Measuring caches, logs and leftovers")
                .child(&spinner())
                .build(),
            Some("analyzing"),
        );

        // Results: total and Clean button above the list.
        let total = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["title-1", "numeric"])
            .build();
        let total_caption = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dim-label"])
            .build();
        let totals = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        totals.append(&total);
        totals.append(&total_caption);

        let rescan = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Scan again")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            rescan.connect_clicked(move |_| sender.input(JunkMsg::Analyze));
        }
        let clean_button = pill("Clean…", true);
        clean_button.set_valign(gtk::Align::Center);
        {
            let sender = sender.clone();
            clean_button.connect_clicked(move |_| sender.input(JunkMsg::AskClean));
        }
        let header = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(18)
            .margin_end(18)
            .build();
        header.append(&totals);
        header.append(&rescan);
        header.append(&clean_button);

        let results_page = adw::PreferencesPage::new();
        let results = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        results.append(
            &adw::Clamp::builder()
                .maximum_size(800)
                .child(&header)
                .build(),
        );
        results.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        results_page.set_vexpand(true);
        results.append(&results_page);
        stack.add_named(&results, Some("results"));

        // Cleaning
        let progress_bar = gtk::ProgressBar::builder().width_request(280).build();
        let progress_label = gtk::Label::builder()
            .css_classes(["dim-label", "numeric"])
            .build();
        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        progress_box.append(&progress_bar);
        progress_box.append(&progress_label);
        stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("user-trash-symbolic")
                .title("Cleaning…")
                .child(&progress_box)
                .build(),
            Some("cleaning"),
        );

        // Done
        let done_details = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let done_button = pill("Scan Again", false);
        {
            let sender = sender.clone();
            done_button.connect_clicked(move |_| sender.input(JunkMsg::Analyze));
        }
        let done_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        done_box.append(&done_details);
        done_box.append(&done_button);
        let done_status = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .child(
                &adw::Clamp::builder()
                    .maximum_size(560)
                    .child(&done_box)
                    .build(),
            )
            .build();
        stack.add_named(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&done_status)
                .build(),
            Some("done"),
        );

        stack.set_visible_child_name("intro");
        let model = JunkPage {
            settings,
            cleaners: builtin_cleaners(),
            state: State::Intro,
            choices: HashMap::new(),
            tick: None,
        };
        let widgets = JunkWidgets {
            total,
            total_caption,
            clean_button,
            results_page,
            groups: Vec::new(),
            progress_bar,
            progress_label,
            done_status,
            done_details,
        };
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
            JunkMsg::Analyze => {
                if matches!(self.state, State::Analyzing | State::Cleaning(_)) {
                    return;
                }
                self.state = State::Analyzing;
                stack.set_visible_child_name("analyzing");
                let cleaners = self.cleaners.clone();
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                let options = AnalyzeOptions {
                    keep_package_versions: self.settings.keep_package_versions,
                };
                sender.spawn_oneshot_command(move || {
                    let Some(places) = Places::current() else {
                        return JunkCmd::Analyzed(Vec::new());
                    };
                    let running = running_processes();
                    JunkCmd::Analyzed(analyze(&cleaners, &places, &guard, &running, &options))
                });
            }
            JunkMsg::Toggle(id, on) => {
                self.choices.insert(id, on);
                self.update_total(w);
            }
            JunkMsg::AskClean => self.ask_clean(stack, &sender),
            JunkMsg::Clean => {
                let State::Results(findings) = &self.state else {
                    return;
                };
                let chosen: Vec<Finding> = findings
                    .iter()
                    .filter(|f| f.can_clean() && self.is_chosen(f))
                    .cloned()
                    .collect();
                if chosen.is_empty() {
                    return;
                }
                let progress = Arc::new(CleanProgress::default());
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                let keep = self.settings.keep_package_versions;
                {
                    let progress = progress.clone();
                    sender.spawn_oneshot_command(move || {
                        JunkCmd::Cleaned(clean_everything(&chosen, &guard, &progress, keep))
                    });
                }
                let tick_sender = sender.clone();
                self.tick = Some(glib::timeout_add_local(
                    Duration::from_millis(100),
                    move || {
                        tick_sender.input(JunkMsg::Tick);
                        glib::ControlFlow::Continue
                    },
                ));
                w.progress_bar.set_fraction(0.0);
                w.progress_label.set_label("Starting…");
                self.state = State::Cleaning(progress);
                stack.set_visible_child_name("cleaning");
            }
            JunkMsg::Tick => {
                if let State::Cleaning(p) = &self.state {
                    if p.waiting_for_admin() {
                        w.progress_bar.pulse();
                        w.progress_label
                            .set_label("Cleaning system files. Enter your password if asked…");
                        return;
                    }
                    let total = p.total().max(1);
                    w.progress_bar.set_fraction(p.done() as f64 / total as f64);
                    w.progress_label.set_label(&format!(
                        "{} of {} items",
                        count(p.done()),
                        count(p.total())
                    ));
                }
            }
            JunkMsg::SettingsChanged(settings) => self.settings = settings,
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
            JunkCmd::Analyzed(findings) => {
                self.state = State::Results(findings);
                self.show_results(w, &sender);
                stack.set_visible_child_name("results");
            }
            JunkCmd::Cleaned(report) => {
                if let Some(id) = self.tick.take() {
                    id.remove();
                }
                self.state = State::Done;
                show_report(w, &report);
                stack.set_visible_child_name("done");
                let _ = sender.output(JunkOutput::Cleaned);
            }
        }
    }
}

impl JunkPage {
    fn is_chosen(&self, f: &Finding) -> bool {
        self.choices
            .get(&f.cleaner.id)
            .copied()
            .unwrap_or(f.cleaner.default_on)
    }

    fn findings(&self) -> &[Finding] {
        match &self.state {
            State::Results(f) => f,
            _ => &[],
        }
    }

    fn update_total(&self, w: &JunkWidgets) {
        let findings = self.findings();
        let chosen: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.can_clean() && self.is_chosen(f))
            .collect();
        let size: u64 = chosen.iter().map(|f| f.size).sum();
        w.total.set_label(&human_size(size));

        let mut caption = if chosen.is_empty() {
            "Nothing selected".to_owned()
        } else {
            format!(
                "selected to clean, from {} {}",
                chosen.len(),
                if chosen.len() == 1 { "place" } else { "places" }
            )
        };
        if chosen.iter().any(|f| f.needs_root()) {
            caption.push_str(" · asks for your password");
        }
        w.total_caption.set_label(&caption);
        w.clean_button.set_sensitive(!chosen.is_empty());
    }

    fn show_results(&self, w: &mut JunkWidgets, sender: &ComponentSender<Self>) {
        for group in w.groups.drain(..) {
            w.results_page.remove(&group);
        }
        let findings = self.findings();
        let home = dirs_home();

        for category in Category::ALL {
            let mut in_group: Vec<&Finding> = findings
                .iter()
                .filter(|f| f.cleaner.category == category && f.status != Status::Empty)
                .collect();
            if in_group.is_empty() {
                continue;
            }
            in_group.sort_by_key(|f| std::cmp::Reverse(f.size));
            let group_size: u64 = in_group.iter().map(|f| f.size).sum();
            let group = adw::PreferencesGroup::builder()
                // Group titles are markup; "Trash & History" needs escaping.
                .title(glib::markup_escape_text(category.title()))
                .description(human_size(group_size))
                .build();
            for f in in_group {
                group.add(&self.finding_row(f, home.as_deref(), sender));
            }
            w.results_page.add(&group);
            w.groups.push(group);
        }

        if w.groups.is_empty() {
            let group = adw::PreferencesGroup::new();
            group.add(
                &adw::ActionRow::builder()
                    .title("No junk found")
                    .subtitle("Your system is already clean.")
                    .build(),
            );
            w.results_page.add(&group);
            w.groups.push(group);
        }
        self.update_total(w);
    }

    fn finding_row(
        &self,
        f: &Finding,
        home: Option<&Path>,
        sender: &ComponentSender<Self>,
    ) -> adw::ExpanderRow {
        let row = {
            let row = adw::ExpanderRow::builder().use_markup(false).build();
            row.set_title(&f.cleaner.name);
            row
        };

        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .build();
        let mut subtitle = f.cleaner.description.clone();
        match &f.status {
            Status::Ready => {
                check.set_active(self.is_chosen(f));
                let sender = sender.clone();
                let id = f.cleaner.id.clone();
                check.connect_toggled(move |c| {
                    sender.input(JunkMsg::Toggle(id.clone(), c.is_active()))
                });
            }
            Status::AppRunning(_) => {
                check.set_sensitive(false);
                subtitle = format!("Close {} to clean this, then scan again.", f.cleaner.name);
                row.add_css_class("dim-label");
            }
            Status::NeedsRoot => {
                check.set_active(self.is_chosen(f));
                let sender = sender.clone();
                let id = f.cleaner.id.clone();
                check.connect_toggled(move |c| {
                    sender.input(JunkMsg::Toggle(id.clone(), c.is_active()))
                });
                subtitle.push_str(" Asks for your password.");
            }
            Status::Empty => {}
        }
        if let Some(note) = &f.note {
            subtitle.push(' ');
            subtitle.push_str(note);
        }
        row.set_subtitle(&subtitle);
        check.update_property(&[gtk::accessible::Property::Label(&format!(
            "Clean {}",
            f.cleaner.name
        ))]);
        row.add_prefix(&check);
        if let Some(icon) = &f.cleaner.icon {
            row.add_prefix(&gtk::Image::from_icon_name(icon));
        }
        row.add_suffix(
            &gtk::Label::builder()
                .label(human_size(f.size))
                .css_classes(["numeric"])
                .valign(gtk::Align::Center)
                .build(),
        );

        // Preview: exactly what would be removed.
        for item in f.items.iter().take(MAX_ITEMS) {
            let item_row = {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&short_path(&item.path, home));
                row.set_subtitle(
                    &(if item.files == 1 {
                        "1 file".to_owned()
                    } else {
                        format!("{} files", count(item.files))
                    }),
                );
                row
            };
            item_row.add_suffix(
                &gtk::Label::builder()
                    .label(human_size(item.size))
                    .css_classes(["numeric", "dim-label"])
                    .build(),
            );
            row.add_row(&item_row);
        }
        if f.items.len() > MAX_ITEMS {
            let rest = &f.items[MAX_ITEMS..];
            row.add_row(
                &adw::ActionRow::builder()
                    .title(format!(
                        "and {} more, {} in total",
                        count(rest.len() as u64),
                        human_size(rest.iter().map(|i| i.size).sum())
                    ))
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        row
    }

    fn ask_clean(&self, parent: &gtk::Stack, sender: &ComponentSender<Self>) {
        let chosen: Vec<&Finding> = self
            .findings()
            .iter()
            .filter(|f| f.can_clean() && self.is_chosen(f))
            .collect();
        if chosen.is_empty() {
            return;
        }
        let size: u64 = chosen.iter().map(|f| f.size).sum();
        let mut names: Vec<String> = chosen.iter().map(|f| f.cleaner.name.clone()).collect();
        if names.len() > 8 {
            let more = names.len() - 7;
            names.truncate(7);
            names.push(format!("and {more} more"));
        }
        let permanent_loss = chosen
            .iter()
            .any(|f| f.cleaner.category == Category::TrashHistory);

        let mut body = format!(
            "{}.\n\nCaches are deleted permanently; apps recreate them when needed.",
            names.join(", ")
        );
        if permanent_loss {
            body.push_str("\n\nEmptying the Trash or clearing history can't be undone.");
        }
        let system: Vec<&str> = chosen
            .iter()
            .filter(|f| f.needs_root())
            .map(|f| f.cleaner.name.as_str())
            .collect();
        if !system.is_empty() {
            body.push_str(&format!(
                "\n\nYou'll be asked for your password to clean system files: {}.",
                system.join(", ")
            ));
        }
        let dialog =
            adw::AlertDialog::new(Some(&format!("Clean {}?", human_size(size))), Some(&body));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("clean", "Clean");
        dialog.set_response_appearance(
            "clean",
            if permanent_loss {
                adw::ResponseAppearance::Destructive
            } else {
                adw::ResponseAppearance::Suggested
            },
        );
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let sender = sender.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "clean" {
                sender.input(JunkMsg::Clean);
            }
        });
        dialog.present(Some(parent));
    }
}

/// Cleans the user's files, then runs the system cleanups through the
/// administrator helper (one password prompt for all of them).
fn clean_everything(
    chosen: &[Finding],
    guard: &PathGuard,
    progress: &CleanProgress,
    keep: u32,
) -> CleanReport {
    let mut report = clean(chosen, guard, progress);
    let system: Vec<(&Finding, Operation)> = chosen
        .iter()
        .filter(|f| f.needs_root())
        .filter_map(|f| root_operation(f, keep).map(|op| (f, op)))
        .collect();
    if system.is_empty() {
        return report;
    }
    progress.set_waiting_for_admin(true);
    let ops: Vec<Operation> = system.iter().map(|(_, op)| op.clone()).collect();
    match run_privileged(&ops) {
        Ok(reports) => {
            for r in reports {
                let name = system
                    .iter()
                    .find(|(_, op)| op.name() == r.operation)
                    .map(|(f, _)| f.cleaner.name.clone())
                    .unwrap_or_else(|| r.operation.clone());
                match r.error {
                    None => {
                        report.freed += r.freed;
                        report.removed += 1;
                    }
                    Some(e) => report.failures.push((PathBuf::from(name), e)),
                }
            }
        }
        Err(e) => report.failures.push((PathBuf::from("System files"), e)),
    }
    progress.set_waiting_for_admin(false);
    report
}

fn show_report(w: &JunkWidgets, report: &CleanReport) {
    w.done_status
        .set_title(&format!("Freed {}", human_size(report.freed)));
    w.done_status.set_description(Some(&format!(
        "Removed {} {}.",
        count(report.removed),
        if report.removed == 1 { "item" } else { "items" }
    )));
    w.done_details.remove_all();
    for name in &report.skipped_running {
        let row = {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(&format!("Skipped {name}"));
            row.set_subtitle("It was opened while cleaning. Close it and clean again.");
            row
        };
        row.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        w.done_details.append(&row);
    }
    if !report.failures.is_empty() {
        let failed = {
            let row = adw::ExpanderRow::builder().use_markup(false).build();
            row.set_title(
                &(format!(
                    "{} items couldn't be removed",
                    count(report.failures.len() as u64)
                )),
            );
            row.set_subtitle("Usually because they belong to another user or the administrator.");
            row
        };
        failed.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        for (path, reason) in report.failures.iter().take(MAX_ITEMS) {
            failed.add_row(&{
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&path.display().to_string());
                row.set_subtitle(reason);
                row
            });
        }
        w.done_details.append(&failed);
    }
    w.done_details
        .set_visible(w.done_details.first_child().is_some());
}

fn pill(label: &str, suggested: bool) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .build();
    if suggested {
        button.add_css_class("suggested-action");
    }
    button
}

fn spinner() -> gtk::Spinner {
    gtk::Spinner::builder()
        .spinning(true)
        .width_request(48)
        .height_request(48)
        .halign(gtk::Align::Center)
        .build()
}

fn short_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}
