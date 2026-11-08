//! Settings screen. Every change is saved straight away.

use std::path::PathBuf;

use lcc_core::safety::PathGuard;
use lcc_core::settings::{DeleteMode, Settings, Theme};
use relm4::adw::prelude::*;
use relm4::gtk::gio;
use relm4::{ComponentParts, ComponentSender, SimpleComponent, adw, gtk};

const THEMES: [(Theme, &str); 3] = [
    (Theme::System, "Follow system"),
    (Theme::Light, "Light"),
    (Theme::Dark, "Dark"),
];

const DELETE_MODES: [(DeleteMode, &str); 2] = [
    (DeleteMode::MoveToTrash, "Move to Trash (recommended)"),
    (DeleteMode::Permanent, "Delete permanently"),
];

pub struct SettingsPage {
    settings: Settings,
}

#[derive(Debug)]
pub enum SettingsMsg {
    Theme(Theme),
    DeleteMode(DeleteMode),
    KeepVersions(u32),
    OneFilesystem(bool),
    ShowHidden(bool),
    TrackUsage(bool),
    AddExcluded(PathBuf),
    RemoveExcluded(PathBuf),
}

#[derive(Debug)]
pub enum SettingsOutput {
    /// Settings changed and were saved.
    Changed(Settings),
    /// Something went wrong; show it to the user.
    Error(String),
}

pub struct SettingsWidgets {
    theme: adw::ComboRow,
    delete_mode: adw::ComboRow,
    keep_versions: adw::SpinRow,
    one_filesystem: adw::SwitchRow,
    show_hidden: adw::SwitchRow,
    track_usage: adw::SwitchRow,
    excluded_group: adw::PreferencesGroup,
    excluded_rows: Vec<adw::ActionRow>,
    empty_row: adw::ActionRow,
    shown_excluded: Option<Vec<PathBuf>>,
}

impl SimpleComponent for SettingsPage {
    type Init = Settings;
    type Input = SettingsMsg;
    type Output = SettingsOutput;
    type Root = adw::PreferencesPage;
    type Widgets = SettingsWidgets;

    fn init_root() -> Self::Root {
        adw::PreferencesPage::new()
    }

    fn init(
        settings: Self::Init,
        page: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        // Appearance
        let appearance = adw::PreferencesGroup::builder().title("Appearance").build();
        let theme = combo_row("Style", None, THEMES.iter().map(|(_, l)| *l));
        {
            let sender = sender.clone();
            theme.connect_selected_notify(move |row| {
                if let Some((t, _)) = THEMES.get(row.selected() as usize) {
                    sender.input(SettingsMsg::Theme(*t));
                }
            });
        }
        appearance.add(&theme);
        page.add(&appearance);

        // Cleaning
        let cleaning = adw::PreferencesGroup::builder().title("Cleaning").build();
        let delete_mode = combo_row(
            "Removed files",
            Some("Caches are always deleted directly, since trashing them frees no space"),
            DELETE_MODES.iter().map(|(_, l)| *l),
        );
        {
            let sender = sender.clone();
            delete_mode.connect_selected_notify(move |row| {
                if let Some((m, _)) = DELETE_MODES.get(row.selected() as usize) {
                    sender.input(SettingsMsg::DeleteMode(*m));
                }
            });
        }
        cleaning.add(&delete_mode);

        let keep_versions = adw::SpinRow::with_range(0.0, 10.0, 1.0);
        keep_versions.set_title("Package versions to keep");
        keep_versions.set_subtitle(
            "For pacman. Other package managers remove all downloaded packages when cleaning",
        );
        {
            let sender = sender.clone();
            keep_versions.connect_value_notify(move |row| {
                sender.input(SettingsMsg::KeepVersions(row.value().round() as u32));
            });
        }
        cleaning.add(&keep_versions);
        page.add(&cleaning);

        // Scanning
        let scanning = adw::PreferencesGroup::builder().title("Scanning").build();
        let one_filesystem = switch_row(
            "Stay on one disk",
            "Don't scan into other mounted disks or network shares",
        );
        {
            let sender = sender.clone();
            one_filesystem.connect_active_notify(move |row| {
                sender.input(SettingsMsg::OneFilesystem(row.is_active()));
            });
        }
        let show_hidden = switch_row(
            "Include hidden files",
            "Count files and folders whose names start with a dot",
        );
        {
            let sender = sender.clone();
            show_hidden.connect_active_notify(move |row| {
                sender.input(SettingsMsg::ShowHidden(row.is_active()));
            });
        }
        scanning.add(&one_filesystem);
        scanning.add(&show_hidden);
        page.add(&scanning);

        // Privacy
        let privacy = adw::PreferencesGroup::builder().title("Privacy").build();
        let track_usage = switch_row(
            "Track app usage",
            "Runs a tiny background service that notes once a minute which of your apps are open. Data stays on this computer. Used by the App Usage screen.",
        );
        {
            let sender = sender.clone();
            track_usage.connect_active_notify(move |row| {
                sender.input(SettingsMsg::TrackUsage(row.is_active()));
            });
        }
        privacy.add(&track_usage);
        page.add(&privacy);

        // Excluded folders
        let add_button = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .tooltip_text("Exclude a folder")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            add_button.connect_clicked(move |button| pick_folder(button, sender.clone()));
        }
        let excluded_group = adw::PreferencesGroup::builder()
            .title("Excluded folders")
            .description("These folders are never scanned or cleaned.")
            .header_suffix(&add_button)
            .build();
        let empty_row = adw::ActionRow::builder()
            .title("No excluded folders")
            .css_classes(["dim-label"])
            .build();
        excluded_group.add(&empty_row);
        page.add(&excluded_group);

        // Always protected (read-only)
        page.add(&protected_group());

        let model = SettingsPage { settings };
        let mut widgets = SettingsWidgets {
            theme,
            delete_mode,
            keep_versions,
            one_filesystem,
            show_hidden,
            track_usage,
            excluded_group,
            excluded_rows: Vec::new(),
            empty_row,
            shown_excluded: None,
        };
        model.update_view(&mut widgets, sender);
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, sender: ComponentSender<Self>) {
        let mut next = self.settings.clone();
        match msg {
            SettingsMsg::Theme(t) => next.theme = t,
            SettingsMsg::DeleteMode(m) => next.delete_mode = m,
            SettingsMsg::KeepVersions(n) => next.keep_package_versions = n,
            SettingsMsg::OneFilesystem(b) => next.stay_on_one_filesystem = b,
            SettingsMsg::ShowHidden(b) => next.show_hidden_files = b,
            SettingsMsg::TrackUsage(b) => next.track_app_usage = b,
            SettingsMsg::AddExcluded(path) => {
                if !next.excluded_paths.contains(&path) {
                    next.excluded_paths.push(path);
                    next.excluded_paths.sort();
                }
            }
            SettingsMsg::RemoveExcluded(path) => next.excluded_paths.retain(|p| *p != path),
        }

        // Widgets echo our own updates back as notify signals; ignore those.
        if next == self.settings {
            return;
        }
        self.settings = next;

        if let Err(e) = self.settings.save() {
            tracing::error!("{e}");
            let _ = sender.output(SettingsOutput::Error(e.to_string()));
        }
        let _ = sender.output(SettingsOutput::Changed(self.settings.clone()));
    }

    fn update_view(&self, w: &mut Self::Widgets, sender: ComponentSender<Self>) {
        let s = &self.settings;

        let theme_index = THEMES.iter().position(|(t, _)| *t == s.theme).unwrap_or(0) as u32;
        if w.theme.selected() != theme_index {
            w.theme.set_selected(theme_index);
        }
        let mode_index = DELETE_MODES
            .iter()
            .position(|(m, _)| *m == s.delete_mode)
            .unwrap_or(0) as u32;
        if w.delete_mode.selected() != mode_index {
            w.delete_mode.set_selected(mode_index);
        }
        if w.keep_versions.value().round() as u32 != s.keep_package_versions {
            w.keep_versions
                .set_value(f64::from(s.keep_package_versions));
        }
        set_switch(&w.one_filesystem, s.stay_on_one_filesystem);
        set_switch(&w.show_hidden, s.show_hidden_files);
        set_switch(&w.track_usage, s.track_app_usage);

        if w.shown_excluded.as_ref() != Some(&s.excluded_paths) {
            for row in w.excluded_rows.drain(..) {
                w.excluded_group.remove(&row);
            }
            for path in &s.excluded_paths {
                let row = excluded_row(path, &sender);
                w.excluded_group.add(&row);
                w.excluded_rows.push(row);
            }
            w.empty_row.set_visible(s.excluded_paths.is_empty());
            w.shown_excluded = Some(s.excluded_paths.clone());
        }
    }
}

fn combo_row<'a>(
    title: &str,
    subtitle: Option<&str>,
    items: impl Iterator<Item = &'a str>,
) -> adw::ComboRow {
    let items: Vec<&str> = items.collect();
    let row = adw::ComboRow::builder()
        .title(title)
        .model(&gtk::StringList::new(&items))
        .build();
    if let Some(s) = subtitle {
        row.set_subtitle(s);
    }
    row
}

fn switch_row(title: &str, subtitle: &str) -> adw::SwitchRow {
    adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build()
}

fn set_switch(row: &adw::SwitchRow, active: bool) {
    if row.is_active() != active {
        row.set_active(active);
    }
}

fn excluded_row(path: &std::path::Path, sender: &ComponentSender<SettingsPage>) -> adw::ActionRow {
    let row = {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .title_selectable(true)
            .build();
        row.set_title(&path.display().to_string());
        row
    };
    row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));

    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Stop excluding this folder")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let sender = sender.clone();
    let path = path.to_path_buf();
    remove.connect_clicked(move |_| sender.input(SettingsMsg::RemoveExcluded(path.clone())));
    row.add_suffix(&remove);
    row
}

fn pick_folder(button: &gtk::Button, sender: ComponentSender<SettingsPage>) {
    let window = button.root().and_downcast::<gtk::Window>();
    let dialog = gtk::FileDialog::builder()
        .title("Choose a Folder to Exclude")
        .modal(true)
        .build();
    dialog.select_folder(window.as_ref(), None::<&gio::Cancellable>, move |result| {
        match result {
            Ok(folder) => match folder.path() {
                Some(path) => sender.input(SettingsMsg::AddExcluded(path)),
                None => {
                    let _ = sender.output(SettingsOutput::Error(
                        "That folder isn't on this computer, so it can't be excluded.".to_owned(),
                    ));
                }
            },
            // Cancelled or dismissed: nothing to do.
            Err(e) => tracing::debug!("folder picker closed: {e}"),
        }
    });
}

/// Read-only list of what the app never deletes.
fn protected_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Always protected")
        .description("Built-in safety rules. These can't be turned off.")
        .build();

    let home = dirs_home();
    let rules = PathGuard::builtin_rules(&home);

    let trees = adw::ExpanderRow::builder()
        .title("System and private folders")
        .subtitle("Never deleted, including everything inside")
        .build();
    trees.add_prefix(&gtk::Image::from_icon_name("security-high-symbolic"));
    for rule in rules.iter().filter(|r| !r.exact_only) {
        trees.add_row(&rule_row(&rule.path, rule.reason));
    }
    group.add(&trees);

    let folders = adw::ExpanderRow::builder()
        .title("Standard folders")
        .subtitle("The folders themselves are kept; files inside can be cleaned")
        .build();
    folders.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
    for rule in rules.iter().filter(|r| r.exact_only) {
        folders.add_row(&rule_row(&rule.path, "Kept"));
    }
    group.add(&folders);

    let browser = adw::ActionRow::builder()
        .title("Browser passwords, cookies and history")
        .subtitle(PathGuard::sensitive_file_names().join(", "))
        .build();
    browser.add_prefix(&gtk::Image::from_icon_name("web-browser-symbolic"));
    group.add(&browser);

    group
}

fn rule_row(path: &std::path::Path, reason: &str) -> adw::ActionRow {
    {
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_title(&path.display().to_string());
        row.set_subtitle(reason);
        row
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("~"))
}
