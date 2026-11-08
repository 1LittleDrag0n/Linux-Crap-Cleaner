//! The review shown before a folder on another drive is deleted: everything
//! inside it, with sub-folders that open on click, and a check box that must
//! be ticked before the delete button works.

use std::cell::Cell;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use lcc_core::contents::{self, Item, ItemKind};
use lcc_core::format::{count, human_size};
use lcc_core::scan::DirNode;
use lcc_core::settings::DeleteMode;
use relm4::adw::prelude::*;
use relm4::{adw, gtk};

/// Rows shown per folder; the rest are summed up in one row.
const MAX_ROWS: usize = 300;

pub struct Review {
    pub path: PathBuf,
    pub size: u64,
    pub files: u64,
    /// The folder's scan tree, for sub-folder sizes.
    pub node: Option<DirNode>,
    pub mode: DeleteMode,
}

/// Shows the review. `on_delete` runs only after the user ticked the check
/// box and pressed the delete button.
pub fn present(
    review: Review,
    parent: &impl IsA<gtk::Widget>,
    on_open: impl Fn(PathBuf) + 'static,
    on_delete: impl Fn() + 'static,
) {
    let name = review
        .path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| review.path.display().to_string());

    let dialog = adw::Dialog::builder()
        .title(format!("Review “{name}”"))
        .content_width(620)
        .content_height(680)
        .build();

    let page = adw::PreferencesPage::new();

    // What will be deleted.
    let summary = adw::PreferencesGroup::builder()
        .description(match review.mode {
            DeleteMode::MoveToTrash => {
                "This folder is on another drive. Look through what's inside before deleting it. \
                 It goes to that drive's Trash, and space is freed when you empty the Trash."
            }
            DeleteMode::Permanent => {
                "This folder is on another drive. Look through what's inside before deleting it. \
                 It will be deleted permanently and can't be restored."
            }
        })
        .build();
    let folder_row = {
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_title(&review.path.display().to_string());
        row.set_subtitle(
            &(format!(
                "{} in {}",
                human_size(review.size),
                files_text(review.files)
            )),
        );
        row
    };
    folder_row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open in file manager")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    {
        let path = review.path.clone();
        open.connect_clicked(move |_| on_open(path.clone()));
    }
    folder_row.add_suffix(&open);
    summary.add(&folder_row);
    page.add(&summary);

    // Everything inside.
    let inside = adw::PreferencesGroup::builder().title("Inside").build();
    let root = Rc::new(review.node.unwrap_or_default());
    fill(&review.path, &root, &[], |row| inside.add(row));
    page.add(&inside);

    // Confirmation.
    let check =
        gtk::CheckButton::with_label("I've reviewed what's inside and want to delete all of it");
    let cancel = gtk::Button::with_label("Cancel");
    let delete = gtk::Button::builder()
        .label(match review.mode {
            DeleteMode::MoveToTrash => "Move to Trash",
            DeleteMode::Permanent => "Delete Permanently",
        })
        .sensitive(false)
        .css_classes(["destructive-action"])
        .build();
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::End)
        .homogeneous(true)
        .build();
    buttons.append(&cancel);
    buttons.append(&delete);
    let bottom = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    bottom.append(&check);
    bottom.append(&buttons);

    {
        let delete = delete.clone();
        check.connect_toggled(move |c| delete.set_sensitive(c.is_active()));
    }
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }
    {
        let dialog = dialog.clone();
        let check = check.clone();
        delete.connect_clicked(move |_| {
            if check.is_active() {
                dialog.close();
                on_delete();
            }
        });
    }

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    toolbar.add_bottom_bar(&bottom);
    dialog.set_child(Some(&toolbar));
    dialog.set_default_widget(Some(&cancel));
    dialog.present(Some(parent));
}

/// Adds rows for the contents of `folder` (at `location` below the reviewed
/// folder in `root`) through `add`.
fn fill(folder: &Path, root: &Rc<DirNode>, location: &[OsString], add: impl Fn(&gtk::Widget)) {
    let node = location
        .iter()
        .try_fold(root.as_ref(), |n, name| n.child(name));
    let items = match contents::list(folder, node) {
        Ok(items) => items,
        Err(e) => {
            add(note_row(&format!("Couldn't open this folder: {e}")).upcast_ref());
            return;
        }
    };
    if items.is_empty() {
        add(note_row("Empty folder").upcast_ref());
        return;
    }
    for item in items.iter().take(MAX_ROWS) {
        add(&item_row(item, root, location));
    }
    if items.len() > MAX_ROWS {
        let rest = &items[MAX_ROWS..];
        let size: u64 = rest.iter().filter_map(|i| i.size).sum();
        add(note_row(&format!(
            "and {} more items, at least {}. Open the folder to see them all.",
            count(rest.len() as u64),
            human_size(size)
        ))
        .upcast_ref());
    }
}

fn item_row(item: &Item, root: &Rc<DirNode>, location: &[OsString]) -> gtk::Widget {
    let title = item.name.to_string_lossy().into_owned();
    match item.kind {
        ItemKind::Folder => {
            let subtitle = match (item.size, item.files) {
                (Some(size), Some(files)) => {
                    format!("{} in {}", human_size(size), files_text(files))
                }
                _ => "Folder".to_owned(),
            };
            let row = {
                let row = adw::ExpanderRow::builder().use_markup(false).build();
                row.set_title(&title);
                row.set_subtitle(&subtitle);
                row
            };
            row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));

            // Read the sub-folder the first time it's opened.
            let filled = Cell::new(false);
            let path = item.path.clone();
            let root = root.clone();
            let mut child_location = location.to_vec();
            child_location.push(item.name.clone());
            row.connect_expanded_notify(move |row| {
                if row.is_expanded() && !filled.replace(true) {
                    fill(&path, &root, &child_location, |r| row.add_row(r));
                }
            });
            row.upcast()
        }
        ItemKind::File | ItemKind::Link | ItemKind::Other => {
            let (icon, subtitle) = match item.kind {
                ItemKind::File => (
                    "text-x-generic-symbolic",
                    human_size(item.size.unwrap_or(0)),
                ),
                ItemKind::Link => (
                    "insert-link-symbolic",
                    match std::fs::read_link(&item.path) {
                        Ok(target) => {
                            format!("Link to {} (only the link is deleted)", target.display())
                        }
                        Err(_) => "Link (only the link is deleted)".to_owned(),
                    },
                ),
                _ => ("dialog-question-symbolic", "Special file".to_owned()),
            };
            let row = {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&title);
                row.set_subtitle(&subtitle);
                row
            };
            row.add_prefix(&gtk::Image::from_icon_name(icon));
            row.upcast()
        }
    }
}

fn note_row(text: &str) -> adw::ActionRow {
    {
        let row = adw::ActionRow::builder()
            .use_markup(false)
            .css_classes(["dim-label"])
            .build();
        row.set_title(text);
        row
    }
}

fn files_text(files: u64) -> String {
    if files == 1 {
        "1 file".to_owned()
    } else {
        format!("{} files", count(files))
    }
}
