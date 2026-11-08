//! System Leftovers: orphan packages, unused Flatpak runtimes, settings of
//! removed apps, broken launchers and links, unfinished downloads and config
//! files to review. Pick items, confirm, remove.

use std::collections::HashMap;
use std::path::Path;

use lcc_core::format::{ago, count, human_size, now};
use lcc_core::junk::Places;
use lcc_core::leftovers::{Action, Category, Leftover, find_leftovers, flatpak_uninstall};
use lcc_core::privileged::{Operation, run_privileged};
use lcc_core::remove::remove;
use lcc_core::safety::PathGuard;
use lcc_core::settings::{DeleteMode, Settings};
use relm4::adw::prelude::*;
use relm4::gtk::{gio, glib};
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

enum State {
    Intro,
    Searching,
    Results(Vec<Leftover>),
    Removing,
}

pub struct LeftoversPage {
    settings: Settings,
    state: State,
    /// Checkbox choices by item key, kept across rescans.
    choices: HashMap<String, bool>,
}

#[derive(Debug)]
pub enum LeftoversMsg {
    Search,
    Toggle(String, bool),
    AskRemove,
    Remove,
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum LeftoversOutput {
    Toast(String),
}

#[derive(Debug)]
pub enum LeftoversCmd {
    Found(Vec<Leftover>),
    Removed {
        removed: usize,
        failures: Vec<String>,
    },
}

pub struct LeftoversWidgets {
    summary: gtk::Label,
    remove_button: gtk::Button,
    page: adw::PreferencesPage,
    groups: Vec<adw::PreferencesGroup>,
}

/// Stable key for remembering an item's checkbox.
fn key(l: &Leftover) -> String {
    match (&l.action, &l.path) {
        (
            Action::FlatpakUninstall {
                reference,
                installation,
            },
            _,
        ) => {
            format!("flatpak:{installation}:{reference}")
        }
        (_, Some(p)) => format!("path:{}", p.display()),
        _ => format!("{:?}:{}", l.category, l.name),
    }
}

fn removable(l: &Leftover) -> bool {
    matches!(
        l.action,
        Action::Remove | Action::FlatpakUninstall { .. } | Action::RemoveOrphanPackage
    )
}

impl Component for LeftoversPage {
    type Init = Settings;
    type Input = LeftoversMsg;
    type Output = LeftoversOutput;
    type CommandOutput = LeftoversCmd;
    type Root = gtk::Stack;
    type Widgets = LeftoversWidgets;

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
        let search = gtk::Button::builder()
            .label("Look for Leftovers")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        {
            let sender = sender.clone();
            search.connect_clicked(move |_| sender.input(LeftoversMsg::Search));
        }
        stack.add_named(
            &adw::StatusPage::builder()
                .icon_name("package-x-generic-symbolic")
                .title("System Leftovers")
                .description(
                    "Find what updates and uninstalled apps left behind: unused packages, old app settings, broken launchers and more.\nNothing is removed until you've reviewed it.",
                )
                .child(&search)
                .build(),
            Some("intro"),
        );
        stack.add_named(
            &adw::StatusPage::builder()
                .title("Looking for leftovers…")
                .child(
                    &gtk::Spinner::builder()
                        .spinning(true)
                        .width_request(48)
                        .height_request(48)
                        .halign(gtk::Align::Center)
                        .build(),
                )
                .build(),
            Some("searching"),
        );

        let summary = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .hexpand(true)
            .css_classes(["dim-label"])
            .build();
        let rescan = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Look again")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            rescan.connect_clicked(move |_| sender.input(LeftoversMsg::Search));
        }
        let remove_button = gtk::Button::builder()
            .label("Remove…")
            .valign(gtk::Align::Center)
            .css_classes(["pill", "destructive-action"])
            .build();
        {
            let sender = sender.clone();
            remove_button.connect_clicked(move |_| sender.input(LeftoversMsg::AskRemove));
        }
        let header = gtk::Box::builder()
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        header.append(&summary);
        header.append(&rescan);
        header.append(&remove_button);
        let page = adw::PreferencesPage::builder().vexpand(true).build();
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
        results.append(&page);
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
        stack.set_visible_child_name("intro");

        ComponentParts {
            model: LeftoversPage {
                settings,
                state: State::Intro,
                choices: HashMap::new(),
            },
            widgets: LeftoversWidgets {
                summary,
                remove_button,
                page,
                groups: Vec::new(),
            },
        }
    }

    fn update_with_view(
        &mut self,
        w: &mut Self::Widgets,
        msg: Self::Input,
        sender: ComponentSender<Self>,
        stack: &Self::Root,
    ) {
        match msg {
            LeftoversMsg::Search => {
                if matches!(self.state, State::Searching | State::Removing) {
                    return;
                }
                self.state = State::Searching;
                stack.set_visible_child_name("searching");
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                sender.spawn_oneshot_command(move || {
                    let found = Places::current()
                        .map(|places| find_leftovers(&places, &guard))
                        .unwrap_or_default();
                    LeftoversCmd::Found(found)
                });
            }
            LeftoversMsg::Toggle(k, on) => {
                self.choices.insert(k, on);
                self.update_summary(w);
            }
            LeftoversMsg::AskRemove => self.ask_remove(stack, &sender),
            LeftoversMsg::Remove => {
                let chosen = self.chosen().into_iter().cloned().collect::<Vec<_>>();
                if chosen.is_empty() {
                    return;
                }
                self.state = State::Removing;
                stack.set_visible_child_name("removing");
                let mode = self.settings.delete_mode;
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                sender.spawn_oneshot_command(move || remove_all(&chosen, mode, &guard));
            }
            LeftoversMsg::SettingsChanged(s) => self.settings = s,
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
            LeftoversCmd::Found(found) => {
                self.state = State::Results(found);
                self.show_results(w, &sender);
                stack.set_visible_child_name("results");
            }
            LeftoversCmd::Removed { removed, failures } => {
                let mut text = format!(
                    "Removed {} {}",
                    count(removed as u64),
                    if removed == 1 { "item" } else { "items" }
                );
                if let Some(first) = failures.first() {
                    text.push_str(&format!(
                        ". {} couldn't be removed: {first}",
                        count(failures.len() as u64)
                    ));
                }
                let _ = sender.output(LeftoversOutput::Toast(text));
                self.state = State::Intro;
                sender.input(LeftoversMsg::Search);
            }
        }
    }
}

impl LeftoversPage {
    fn items(&self) -> &[Leftover] {
        match &self.state {
            State::Results(items) => items,
            _ => &[],
        }
    }

    fn is_chosen(&self, l: &Leftover) -> bool {
        removable(l) && self.choices.get(&key(l)).copied().unwrap_or(l.default_on)
    }

    fn chosen(&self) -> Vec<&Leftover> {
        self.items().iter().filter(|l| self.is_chosen(l)).collect()
    }

    fn update_summary(&self, w: &LeftoversWidgets) {
        let chosen = self.chosen();
        let size: u64 = chosen.iter().map(|l| l.size).sum();
        let total = self.items().len();
        w.summary.set_label(&if total == 0 {
            "No leftovers found. Your system is tidy.".to_owned()
        } else if chosen.is_empty() {
            format!("{} found · nothing selected", count(total as u64))
        } else {
            format!(
                "{} found · {} selected, {}",
                count(total as u64),
                count(chosen.len() as u64),
                human_size(size)
            )
        });
        w.remove_button.set_sensitive(!chosen.is_empty());
    }

    fn show_results(&self, w: &mut LeftoversWidgets, sender: &ComponentSender<Self>) {
        for g in w.groups.drain(..) {
            w.page.remove(&g);
        }
        let now = now();
        for category in Category::ALL {
            let items: Vec<&Leftover> = self
                .items()
                .iter()
                .filter(|l| l.category == category)
                .collect();
            if items.is_empty() {
                continue;
            }
            let size: u64 = items.iter().map(|l| l.size).sum();
            let mut description = category.description().to_owned();
            if size > 0 {
                description = format!("{} · {description}", human_size(size));
            }
            if items
                .iter()
                .any(|l| l.action == Action::RemoveOrphanPackage)
            {
                description.push_str(" Removing them asks for your password.");
            }
            let group = adw::PreferencesGroup::builder()
                .title(glib::markup_escape_text(category.title()))
                .description(glib::markup_escape_text(&description))
                .build();
            for l in items {
                group.add(&self.row(l, now, sender));
            }
            w.page.add(&group);
            w.groups.push(group);
        }
        self.update_summary(w);
    }

    fn row(&self, l: &Leftover, now: i64, sender: &ComponentSender<Self>) -> adw::ActionRow {
        let mut subtitle = l.detail.clone();
        if let Some(m) = l.modified {
            subtitle.push_str(&format!(" · changed {}", ago(m, now)));
        }
        let row = {
            let row = adw::ActionRow::builder()
                .use_markup(false)
                .subtitle_lines(2)
                .build();
            row.set_title(&l.name);
            row.set_subtitle(&subtitle);
            row
        };

        if removable(l) {
            let check = gtk::CheckButton::builder()
                .valign(gtk::Align::Center)
                .active(self.is_chosen(l))
                .build();
            check.update_property(&[gtk::accessible::Property::Label(&format!(
                "Remove {}",
                l.name
            ))]);
            let sender = sender.clone();
            let k = key(l);
            check.connect_toggled(move |c| {
                sender.input(LeftoversMsg::Toggle(k.clone(), c.is_active()))
            });
            row.add_prefix(&check);
            row.set_activatable_widget(Some(&check));
        }
        if l.size > 0 {
            row.add_suffix(
                &gtk::Label::builder()
                    .label(human_size(l.size))
                    .css_classes(["numeric", "dim-label"])
                    .build(),
            );
        }
        if let Some(path) = &l.path {
            let show = gtk::Button::builder()
                .icon_name("folder-open-symbolic")
                .tooltip_text("Show in file manager")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            let path = path.clone();
            let sender = sender.clone();
            show.connect_clicked(move |b| show_in_files(b, &path, &sender));
            row.add_suffix(&show);
        }
        row
    }

    fn ask_remove(&self, parent: &gtk::Stack, sender: &ComponentSender<Self>) {
        let chosen = self.chosen();
        if chosen.is_empty() {
            return;
        }
        let files = chosen.iter().filter(|l| l.action == Action::Remove).count();
        let packages = chosen
            .iter()
            .filter(|l| l.action == Action::RemoveOrphanPackage)
            .count();
        let runtimes = chosen.len() - files - packages;
        let mut lines = Vec::new();
        if files > 0 {
            lines.push(match self.settings.delete_mode {
                DeleteMode::MoveToTrash => format!(
                    "{} {} will be moved to the Trash, where you can restore them.",
                    count(files as u64),
                    if files == 1 {
                        "file or folder"
                    } else {
                        "files and folders"
                    }
                ),
                DeleteMode::Permanent => format!(
                    "{} {} will be deleted permanently. This can't be undone.",
                    count(files as u64),
                    if files == 1 {
                        "file or folder"
                    } else {
                        "files and folders"
                    }
                ),
            });
        }
        if packages > 0 {
            lines.push(format!(
                "{} {} will be uninstalled with pacman. You'll be asked for your password.",
                count(packages as u64),
                if packages == 1 { "package" } else { "packages" }
            ));
        }
        if runtimes > 0 {
            lines.push(format!(
                "{} Flatpak {} will be uninstalled. Flatpak may ask for your password.",
                count(runtimes as u64),
                if runtimes == 1 { "runtime" } else { "runtimes" }
            ));
        }
        let size: u64 = chosen.iter().map(|l| l.size).sum();
        let dialog = adw::AlertDialog::new(
            Some(&format!(
                "Remove {} {}?",
                count(chosen.len() as u64),
                if chosen.len() == 1 { "item" } else { "items" }
            )),
            Some(&format!("{}\n\n{}", human_size(size), lines.join("\n"))),
        );
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("remove", "Remove");
        dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let sender = sender.clone();
        dialog.connect_response(None, move |_, r| {
            if r == "remove" {
                sender.input(LeftoversMsg::Remove);
            }
        });
        dialog.present(Some(parent));
    }
}

fn remove_all(items: &[Leftover], mode: DeleteMode, guard: &PathGuard) -> LeftoversCmd {
    let mut removed = 0;
    let mut failures = Vec::new();
    for l in items {
        let result = match (&l.action, &l.path) {
            (Action::Remove, Some(path)) => remove(path, mode, guard).map_err(|e| e.to_string()),
            (
                Action::FlatpakUninstall {
                    reference,
                    installation,
                },
                _,
            ) => flatpak_uninstall(reference, installation).map_err(|e| format!("{}: {e}", l.name)),
            _ => continue,
        };
        match result {
            Ok(()) => removed += 1,
            Err(e) => failures.push(e),
        }
    }

    // All orphan packages in one pacman call, so one password prompt.
    let orphans: Vec<String> = items
        .iter()
        .filter(|l| l.action == Action::RemoveOrphanPackage)
        .map(|l| l.name.clone())
        .collect();
    if !orphans.is_empty() {
        let count = orphans.len();
        match run_privileged(&[Operation::RemoveOrphans { packages: orphans }]) {
            Ok(reports) => match reports.into_iter().find_map(|r| r.error) {
                None => removed += count,
                Some(e) => failures.push(e),
            },
            Err(e) => failures.push(e),
        }
    }
    LeftoversCmd::Removed { removed, failures }
}

fn show_in_files(button: &gtk::Button, path: &Path, sender: &ComponentSender<LeftoversPage>) {
    let window = button.root().and_downcast::<gtk::Window>();
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let sender = sender.clone();
    launcher.open_containing_folder(window.as_ref(), None::<&gio::Cancellable>, move |r| {
        if let Err(e) = r
            && !e.matches(gtk::DialogError::Dismissed)
        {
            let _ = sender.output(LeftoversOutput::Toast(format!(
                "Couldn't open the file manager: {e}"
            )));
        }
    });
}
