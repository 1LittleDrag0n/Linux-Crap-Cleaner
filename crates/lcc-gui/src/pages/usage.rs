//! App Usage: which apps you use, and which you haven't opened for months
//! (sorted by size, so the best candidates for removal come first).

use lcc_core::format::{ago, count, human_size, now};
use lcc_core::junk::Places;
use lcc_core::packages::{Package, list_installed};
use lcc_core::settings::Settings;
use lcc_core::usage::{AppUsage, Basis, UsageDb, app_usage};
use relm4::adw::prelude::*;
use relm4::gtk::glib;
use relm4::{Component, ComponentParts, ComponentSender, adw, gtk};

/// Not used for this long counts as unused.
const UNUSED_AFTER: i64 = 90 * 24 * 3600;

pub struct UsagePage {
    settings: Settings,
    apps: Vec<AppUsage>,
    loading: bool,
}

#[derive(Debug)]
pub enum UsageMsg {
    Reload,
    SettingsChanged(Settings),
}

#[derive(Debug)]
pub enum UsageOutput {
    OpenSettings,
    Uninstall(Box<Package>),
}

#[derive(Debug)]
pub enum UsageCmd {
    Loaded(Vec<AppUsage>),
}

pub struct UsageWidgets {
    banner: adw::Banner,
    page: adw::PreferencesPage,
    groups: Vec<adw::PreferencesGroup>,
}

impl Component for UsagePage {
    type Init = Settings;
    type Input = UsageMsg;
    type Output = UsageOutput;
    type CommandOutput = UsageCmd;
    type Root = gtk::Stack;
    type Widgets = UsageWidgets;

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
                .title("Looking at your apps…")
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

        let banner = adw::Banner::builder()
            .title("Times are estimated from files apps change. Turn on tracking for exact data.")
            .button_label("Open Settings")
            .revealed(!settings.track_app_usage)
            .build();
        {
            let sender = sender.clone();
            banner.connect_button_clicked(move |_| {
                let _ = sender.output(UsageOutput::OpenSettings);
            });
        }
        let page = adw::PreferencesPage::builder().vexpand(true).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        content.append(&banner);
        content.append(&page);
        stack.add_named(&content, Some("list"));

        let mut model = UsagePage {
            settings,
            apps: Vec::new(),
            loading: false,
        };
        model.load(&stack, &sender);
        ComponentParts {
            model,
            widgets: UsageWidgets {
                banner,
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
            UsageMsg::Reload => self.load(stack, &sender),
            UsageMsg::SettingsChanged(s) => {
                w.banner.set_revealed(!s.track_app_usage);
                self.settings = s;
            }
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
            UsageCmd::Loaded(apps) => {
                self.loading = false;
                self.apps = apps;
                self.show(w, &sender);
                stack.set_visible_child_name("list");
            }
        }
    }
}

impl UsagePage {
    fn load(&mut self, stack: &gtk::Stack, sender: &ComponentSender<Self>) {
        if self.loading {
            return;
        }
        self.loading = true;
        if self.apps.is_empty() {
            stack.set_visible_child_name("loading");
        }
        sender.spawn_oneshot_command(|| {
            let apps = match Places::current() {
                Some(places) => app_usage(list_installed(), &UsageDb::load(), &places),
                None => Vec::new(),
            };
            UsageCmd::Loaded(apps)
        });
    }

    fn show(&self, w: &mut UsageWidgets, sender: &ComponentSender<Self>) {
        for g in w.groups.drain(..) {
            w.page.remove(&g);
        }
        let now = now();
        let mut unused: Vec<&AppUsage> = Vec::new();
        let mut recent: Vec<&AppUsage> = Vec::new();
        let mut unknown: Vec<&AppUsage> = Vec::new();
        for a in &self.apps {
            match a.last_used {
                None => unknown.push(a),
                Some(t) if now - t >= UNUSED_AFTER => unused.push(a),
                Some(_) => recent.push(a),
            }
        }
        unused.sort_by_key(|a| std::cmp::Reverse(a.package.size));
        unknown.sort_by_key(|a| std::cmp::Reverse(a.package.size));
        recent.sort_by_key(|a| std::cmp::Reverse(a.last_used));

        let reload = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Look again")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let sender = sender.clone();
            reload.connect_clicked(move |_| sender.input(UsageMsg::Reload));
        }

        let sections: [(&str, String, &Vec<&AppUsage>); 3] = [
            (
                "Not Used for 3 Months or More",
                summary(&unused, "Good candidates for removal, largest first."),
                &unused,
            ),
            (
                "No Usage Information",
                summary(
                    &unknown,
                    "Couldn't tell when these were last used. Turning on tracking in Settings will find out.",
                ),
                &unknown,
            ),
            (
                "Used Recently",
                summary(&recent, "Most recently used first."),
                &recent,
            ),
        ];
        let mut first = true;
        for (title, description, items) in sections {
            if items.is_empty() {
                continue;
            }
            let group = adw::PreferencesGroup::builder()
                .title(title)
                .description(glib::markup_escape_text(&description))
                .build();
            if first {
                group.set_header_suffix(Some(&reload));
                first = false;
            }
            for a in items.iter() {
                group.add(&app_row(a, now, sender));
            }
            w.page.add(&group);
            w.groups.push(group);
        }
        if w.groups.is_empty() {
            let group = adw::PreferencesGroup::new();
            group.add(&adw::ActionRow::builder().title("No apps found").build());
            w.page.add(&group);
            w.groups.push(group);
        }
    }
}

fn summary(items: &[&AppUsage], text: &str) -> String {
    let size: u64 = items.iter().map(|a| a.package.size).sum();
    format!(
        "{} {}, {} · {text}",
        count(items.len() as u64),
        if items.len() == 1 { "app" } else { "apps" },
        human_size(size)
    )
}

fn app_row(a: &AppUsage, now: i64, sender: &ComponentSender<UsagePage>) -> adw::ActionRow {
    let p = &a.package;
    let mut subtitle = match (a.last_used, a.basis) {
        (Some(t), Basis::Tracked) => format!("Last used {}", ago(t, now)),
        (Some(t), _) => format!("Last used {} (estimated)", ago(t, now)),
        (None, _) => "No usage information".to_owned(),
    };
    if let Some(minutes) = a.minutes.filter(|m| *m > 0) {
        subtitle.push_str(&format!(" · open for {} in total", duration_words(minutes)));
    }
    subtitle.push_str(&format!(" · {}", p.source.name()));

    let row = adw::ActionRow::builder()
        .use_markup(false)
        .title(&p.display_name)
        .subtitle(subtitle)
        .build();
    let icon = gtk::Image::builder().pixel_size(32).build();
    match p.icon.as_deref() {
        Some(i) if i.starts_with('/') => icon.set_from_file(Some(i)),
        Some(i) => icon.set_icon_name(Some(i)),
        None => icon.set_icon_name(Some("application-x-executable")),
    }
    row.add_prefix(&icon);
    row.add_suffix(
        &gtk::Label::builder()
            .label(human_size(p.size))
            .css_classes(["numeric", "dim-label"])
            .build(),
    );
    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text(p.protected.unwrap_or("Uninstall…"))
        .sensitive(p.protected.is_none())
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let sender = sender.clone();
    let pkg = p.clone();
    remove.connect_clicked(move |_| {
        let _ = sender.output(UsageOutput::Uninstall(Box::new(pkg.clone())));
    });
    row.add_suffix(&remove);
    row
}

fn duration_words(minutes: u64) -> String {
    match minutes {
        0..=59 => format!("{minutes} min"),
        60..=1439 => format!("{} h {} min", minutes / 60, minutes % 60),
        _ => format!("{} days", minutes / 1440),
    }
}
