//! Installed Software: every package, Flatpak and AppImage in one searchable
//! list with sizes, and uninstalling where possible.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use lcc_core::desktop;
use lcc_core::format::{ago, count, human_size, now};
use lcc_core::leftovers::flatpak_uninstall;
use lcc_core::native::{NativePackage, Preview};
use lcc_core::packages::{Package, Source, list_installed};
use lcc_core::privileged::{Operation, run_privileged};
use lcc_core::remove::remove;
use lcc_core::safety::PathGuard;
use lcc_core::settings::{DeleteMode, Settings};
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::typed_view::list::{RelmListItem, TypedListView};
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Show {
    Apps,
    Explicit,
    Everything,
}

const SHOW: [(Show, &str); 3] = [
    (Show::Apps, "Apps"),
    (Show::Explicit, "Installed by you"),
    (Show::Everything, "Everything"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sort {
    Size,
    Name,
    Newest,
}

const SORT: [(Sort, &str); 3] = [
    (Sort::Size, "Largest first"),
    (Sort::Name, "Name"),
    (Sort::Newest, "Recently installed"),
];

#[derive(Debug)]
struct Filter {
    query: String,
    show: Show,
}

fn visible(p: &Package, f: &Filter) -> bool {
    let kind_ok = match f.show {
        Show::Apps => p.is_app,
        Show::Explicit => p.explicit,
        Show::Everything => true,
    };
    if !kind_ok {
        return false;
    }
    if f.query.is_empty() {
        return true;
    }
    let q = f.query.to_lowercase();
    [&p.display_name, &p.name, &p.description]
        .iter()
        .any(|s| s.to_lowercase().contains(&q))
}

/// One row in the list.
struct PackageRow {
    pkg: Package,
    sender: ComponentSender<SoftwarePage>,
}

struct PackageWidgets {
    icon: gtk::Image,
    title: gtk::Label,
    subtitle: gtk::Label,
    tag: gtk::Label,
    size: gtk::Label,
    remove: gtk::Button,
    handler: Option<glib::SignalHandlerId>,
}

impl RelmListItem for PackageRow {
    type Root = gtk::Box;
    type Widgets = PackageWidgets;

    fn setup(_: &gtk::ListItem) -> (gtk::Box, PackageWidgets) {
        let root = gtk::Box::builder()
            .spacing(12)
            .margin_top(8)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        let icon = gtk::Image::builder().pixel_size(32).build();
        let title = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["heading"])
            .build();
        let subtitle = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label", "caption"])
            .build();
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        text.append(&title);
        text.append(&subtitle);
        let tag = gtk::Label::builder()
            .css_classes(["caption", "dim-label"])
            .valign(gtk::Align::Center)
            .build();
        let size = gtk::Label::builder()
            .css_classes(["numeric"])
            .width_chars(9)
            .xalign(1.0)
            .valign(gtk::Align::Center)
            .build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        root.append(&icon);
        root.append(&text);
        root.append(&tag);
        root.append(&size);
        root.append(&remove);
        (
            root,
            PackageWidgets {
                icon,
                title,
                subtitle,
                tag,
                size,
                remove,
                handler: None,
            },
        )
    }

    fn bind(&mut self, w: &mut PackageWidgets, _: &mut gtk::Box) {
        let p = &self.pkg;
        match p.icon.as_deref() {
            Some(icon) if icon.starts_with('/') => w.icon.set_from_file(Some(icon)),
            Some(icon) => w.icon.set_icon_name(Some(icon)),
            None => w.icon.set_icon_name(Some(if p.is_app {
                "application-x-executable"
            } else {
                "package-x-generic"
            })),
        }
        w.title.set_label(&p.display_name);
        let mut subtitle = if p.description.is_empty() {
            p.name.clone()
        } else if p.display_name != p.name {
            format!("{} · {}", p.name, p.description)
        } else {
            p.description.clone()
        };
        if let Some(t) = p.installed {
            subtitle = format!("{subtitle} · installed {}", ago(t, now()));
        }
        w.subtitle.set_label(&subtitle);
        w.subtitle.set_tooltip_text(Some(&subtitle));
        w.tag
            .set_label(&if p.explicit || !matches!(p.source, Source::Native(_)) {
                p.source.name().to_owned()
            } else {
                format!("{} · dependency", p.source.name())
            });
        w.size.set_label(&human_size(p.size));

        let (sensitive, tooltip) = match (p.source, p.protected) {
            (_, Some(why)) => (false, why),
            (Source::Native(_) | Source::Flatpak | Source::Snap, None) => (true, "Uninstall…"),
            (Source::AppImage, None) => (true, "Remove…"),
        };
        w.remove.set_sensitive(sensitive);
        w.remove.set_tooltip_text(Some(tooltip));
        if let Some(h) = w.handler.take() {
            w.remove.disconnect(h);
        }
        let sender = self.sender.clone();
        let pkg = p.clone();
        w.handler = Some(w.remove.connect_clicked(move |_| {
            sender.input(SoftwareMsg::AskUninstall(pkg.clone()));
        }));
    }

    fn unbind(&mut self, w: &mut PackageWidgets, _: &mut gtk::Box) {
        if let Some(h) = w.handler.take() {
            w.remove.disconnect(h);
        }
    }
}

pub struct SoftwarePage {
    settings: Settings,
    all: Vec<Package>,
    list: TypedListView<PackageRow, gtk::NoSelection>,
    filter: Rc<RefCell<Filter>>,
    sort: Sort,
    loading: bool,
}

#[derive(Debug)]
pub enum SoftwareMsg {
    Reload,
    Search(String),
    Show(u32),
    Sort(u32),
    AskUninstall(Package),
    Uninstall(Package),
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum SoftwareOutput {
    Toast(String),
    /// Something was uninstalled.
    Changed,
}

#[derive(Debug)]
pub enum SoftwareCmd {
    Loaded(Vec<Package>),
    /// What uninstalling a system package would remove.
    Preview {
        pkg: Package,
        result: Result<Preview, String>,
    },
    Uninstalled {
        name: String,
        result: Result<(), String>,
    },
}

pub struct SoftwareWidgets {
    summary: gtk::Label,
}

impl Component for SoftwarePage {
    type Init = Settings;
    type Input = SoftwareMsg;
    type Output = SoftwareOutput;
    type CommandOutput = SoftwareCmd;
    type Root = gtk::Stack;
    type Widgets = SoftwareWidgets;

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
        stack.add_named(
            &adw::StatusPage::builder()
                .title("Reading installed software…")
                .child(
                    &gtk::Spinner::builder()
                        .spinning(true)
                        .width_request(48)
                        .height_request(48)
                        .halign(gtk::Align::Center)
                        .build(),
                )
                .build(),
            Some("loading"),
        );

        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search installed software")
            .hexpand(true)
            .build();
        {
            let sender = sender.clone();
            search.connect_search_changed(move |e| {
                sender.input(SoftwareMsg::Search(e.text().to_string()))
            });
        }
        let show = gtk::DropDown::from_strings(&SHOW.map(|(_, l)| l));
        show.set_tooltip_text(Some("Show"));
        {
            let sender = sender.clone();
            show.connect_selected_notify(move |d| sender.input(SoftwareMsg::Show(d.selected())));
        }
        let sort = gtk::DropDown::from_strings(&SORT.map(|(_, l)| l));
        sort.set_tooltip_text(Some("Sort by"));
        {
            let sender = sender.clone();
            sort.connect_selected_notify(move |d| sender.input(SoftwareMsg::Sort(d.selected())));
        }
        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reload")
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            reload.connect_clicked(move |_| sender.input(SoftwareMsg::Reload));
        }
        let controls = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(4)
            .min_children_per_line(1)
            .column_spacing(6)
            .row_spacing(6)
            .homogeneous(false)
            .build();
        let search_box = gtk::Box::builder().hexpand(true).build();
        search_box.append(&search);
        search_box.set_size_request(240, -1);
        controls.append(&search_box);
        controls.append(&show);
        controls.append(&sort);
        controls.append(&reload);

        let summary = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();

        let filter = Rc::new(RefCell::new(Filter {
            query: String::new(),
            show: Show::Apps,
        }));
        let mut list: TypedListView<PackageRow, gtk::NoSelection> = TypedListView::new();
        {
            let filter = filter.clone();
            list.add_filter(move |row| visible(&row.pkg, &filter.borrow()));
        }
        list.view.set_show_separators(true);
        list.view.add_css_class("card");

        let header = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(6)
            .margin_start(12)
            .margin_end(12)
            .build();
        header.append(&controls);
        header.append(&summary);

        // The list view must sit directly in the scrolled window so only
        // visible rows are built; ClampScrollable keeps it readable on wide
        // windows without breaking that.
        list.view.set_margin_start(12);
        list.view.set_margin_end(12);
        list.view.set_margin_bottom(12);
        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        page.append(
            &adw::Clamp::builder()
                .maximum_size(960)
                .child(&header)
                .build(),
        );
        page.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::ClampScrollable::builder()
                        .maximum_size(960)
                        .child(&list.view)
                        .build(),
                )
                .vexpand(true)
                .build(),
        );
        stack.add_named(&page, Some("list"));

        let mut model = SoftwarePage {
            settings,
            all: Vec::new(),
            list,
            filter,
            sort: Sort::Size,
            loading: false,
        };
        model.load(&stack, &sender);
        ComponentParts {
            model,
            widgets: SoftwareWidgets { summary },
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
            SoftwareMsg::Reload => self.load(stack, &sender),
            SoftwareMsg::Search(q) => {
                self.filter.borrow_mut().query = q;
                self.list.notify_filter_changed(0);
                self.update_summary(w);
            }
            SoftwareMsg::Show(i) => {
                if let Some((show, _)) = SHOW.get(i as usize) {
                    self.filter.borrow_mut().show = *show;
                    self.list.notify_filter_changed(0);
                    self.update_summary(w);
                }
            }
            SoftwareMsg::Sort(i) => {
                if let Some((sort, _)) = SORT.get(i as usize) {
                    self.sort = *sort;
                    self.apply_sort();
                }
            }
            SoftwareMsg::AskUninstall(pkg) => self.ask_uninstall(pkg, stack, &sender),
            SoftwareMsg::Uninstall(pkg) => {
                let mode = self.settings.delete_mode;
                let guard = PathGuard::for_current_user(&self.settings.excluded_paths);
                sender.spawn_oneshot_command(move || {
                    let result = uninstall(&pkg, mode, &guard);
                    SoftwareCmd::Uninstalled {
                        name: pkg.display_name,
                        result,
                    }
                });
            }
            SoftwareMsg::SettingsChanged(s) => self.settings = s,
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
            SoftwareCmd::Loaded(packages) => {
                self.loading = false;
                self.list.clear();
                self.list
                    .extend_from_iter(packages.iter().cloned().map(|pkg| PackageRow {
                        pkg,
                        sender: sender.clone(),
                    }));
                self.all = packages;
                self.apply_sort();
                self.update_summary(w);
                stack.set_visible_child_name("list");
            }
            SoftwareCmd::Preview { pkg, result } => match result {
                Ok(preview) => confirm_package_removal(pkg, &preview, stack, &sender),
                Err(e) => {
                    let dialog = adw::AlertDialog::new(
                        Some(&format!("Can't uninstall {}", pkg.display_name)),
                        Some(&e),
                    );
                    dialog.add_response("ok", "OK");
                    dialog.present(Some(stack));
                }
            },
            SoftwareCmd::Uninstalled { name, result } => {
                let text = match result {
                    Ok(()) => {
                        let _ = sender.output(SoftwareOutput::Changed);
                        format!("Removed {name}")
                    }
                    Err(e) => format!("Couldn't remove {name}: {e}"),
                };
                let _ = sender.output(SoftwareOutput::Toast(text));
                self.load(stack, &sender);
            }
        }
    }
}

impl SoftwarePage {
    fn load(&mut self, stack: &gtk::Stack, sender: &ComponentSender<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        if self.all.is_empty() {
            stack.set_visible_child_name("loading");
        }
        sender.spawn_oneshot_command(|| SoftwareCmd::Loaded(list_installed()));
    }

    fn apply_sort(&self) {
        match self.sort {
            Sort::Size => self.list.sort(|a, b| b.pkg.size.cmp(&a.pkg.size)),
            Sort::Name => self.list.sort(|a, b| {
                a.pkg
                    .display_name
                    .to_lowercase()
                    .cmp(&b.pkg.display_name.to_lowercase())
            }),
            Sort::Newest => self.list.sort(|a, b| b.pkg.installed.cmp(&a.pkg.installed)),
        }
    }

    fn update_summary(&self, w: &SoftwareWidgets) {
        let f = self.filter.borrow();
        let shown: Vec<&Package> = self.all.iter().filter(|p| visible(p, &f)).collect();
        let size: u64 = shown.iter().map(|p| p.size).sum();
        w.summary.set_label(&format!(
            "{} shown, {} in total · {} installed altogether",
            count(shown.len() as u64),
            human_size(size),
            count(self.all.len() as u64)
        ));
    }

    fn ask_uninstall(&self, pkg: Package, parent: &gtk::Stack, sender: &ComponentSender<Self>) {
        let (heading, body, action) = match pkg.source {
            Source::Native(backend) => {
                let names = vec![pkg.name.clone()];
                // Sizes for the preview, from what's already listed.
                let installed: Vec<NativePackage> = self
                    .all
                    .iter()
                    .filter(|p| p.source == pkg.source)
                    .map(|p| NativePackage {
                        name: p.name.clone(),
                        size: p.size,
                        ..NativePackage::default()
                    })
                    .collect();
                sender.spawn_oneshot_command(move || SoftwareCmd::Preview {
                    result: backend.preview_removal(&names, &installed),
                    pkg,
                });
                return;
            }
            Source::Snap => (
                format!("Uninstall {}?", pkg.display_name),
                format!(
                    "Removes the snap ({}). You'll be asked for your password.",
                    human_size(pkg.size)
                ),
                "Uninstall",
            ),
            Source::Flatpak => (
                format!("Uninstall {}?", pkg.display_name),
                format!(
                    "Removes the app ({}). Its settings and saved data stay in ~/.var/app; System Leftovers can remove them later.",
                    human_size(pkg.size)
                ),
                "Uninstall",
            ),
            Source::AppImage => {
                let path = pkg.path.clone().unwrap_or_default();
                let launchers = launchers_for(&path).len();
                let what = match self.settings.delete_mode {
                    DeleteMode::MoveToTrash => "Moves it to the Trash",
                    DeleteMode::Permanent => "Deletes it permanently",
                };
                let extra = match launchers {
                    0 => String::new(),
                    1 => ", together with its menu launcher".to_owned(),
                    n => format!(", together with its {n} menu launchers"),
                };
                (
                    format!("Remove {}?", pkg.display_name),
                    format!("{what}{extra}.\n{}", path.display()),
                    "Remove",
                )
            }
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
                sender.input(SoftwareMsg::Uninstall(pkg.clone()));
            }
        });
        dialog.present(Some(parent));
    }
}

fn confirm_package_removal(
    pkg: Package,
    preview: &Preview,
    parent: &gtk::Stack,
    sender: &ComponentSender<SoftwarePage>,
) {
    let removed = &preview.packages;
    let size: u64 = removed.iter().map(|(_, s)| s).sum();
    let others: Vec<&str> = removed
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| *n != pkg.name)
        .collect();
    let mut body = if preview.complete {
        format!("Frees {}.", human_size(size))
    } else {
        format!("Frees at least {}.", human_size(size))
    };
    if !others.is_empty() {
        let shown: Vec<&str> = others.iter().take(12).copied().collect();
        let more = others.len().saturating_sub(shown.len());
        body.push_str(&format!(
            "\n\nAlso removes {} {} only it needed: {}{}.",
            count(others.len() as u64),
            if others.len() == 1 {
                "package"
            } else {
                "packages"
            },
            shown.join(", "),
            if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            }
        ));
    }
    if !preview.complete {
        body.push_str("\n\nDependencies that nothing else needs are removed too.");
    }
    body.push_str("\n\nYou'll be asked for your password.");
    let dialog = adw::AlertDialog::new(
        Some(&format!("Uninstall {}?", pkg.display_name)),
        Some(&body),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("remove", "Uninstall");
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let sender = sender.clone();
    dialog.connect_response(None, move |_, response| {
        if response == "remove" {
            sender.input(SoftwareMsg::Uninstall(pkg.clone()));
        }
    });
    dialog.present(Some(parent));
}

/// User launchers that start the AppImage at `path`.
fn launchers_for(path: &Path) -> Vec<PathBuf> {
    let Some(apps) =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share/applications"))
    else {
        return Vec::new();
    };
    let needle = path.to_string_lossy();
    desktop::read_dir_entries(&apps)
        .into_iter()
        .filter(|e| e.exec.as_deref().is_some_and(|x| x.contains(&*needle)))
        .map(|e| e.path)
        .collect()
}

fn uninstall(pkg: &Package, mode: DeleteMode, guard: &PathGuard) -> Result<(), String> {
    match pkg.source {
        Source::Native(_) | Source::Snap => {
            let op = if pkg.source == Source::Snap {
                Operation::RemoveSnaps {
                    names: vec![pkg.name.clone()],
                }
            } else {
                Operation::RemovePackages {
                    packages: vec![pkg.name.clone()],
                }
            };
            match run_privileged(&[op])?.into_iter().find_map(|r| r.error) {
                None => Ok(()),
                Some(e) => Err(e),
            }
        }
        Source::Flatpak => {
            flatpak_uninstall(&pkg.name, pkg.installation.as_deref().unwrap_or("system"))
        }
        Source::AppImage => {
            let path = pkg.path.as_deref().ok_or("unknown file")?;
            let launchers = launchers_for(path);
            remove(path, mode, guard).map_err(|e| e.to_string())?;
            for launcher in launchers {
                remove(&launcher, mode, guard).map_err(|e| e.to_string())?;
            }
            Ok(())
        }
    }
}
