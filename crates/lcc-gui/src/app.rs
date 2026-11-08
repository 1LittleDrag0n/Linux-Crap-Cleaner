//! Main window: a sidebar of screens on the left, the chosen screen on the
//! right. On narrow windows the sidebar and content become separate pages.

use std::path::PathBuf;

use lcc_core::packages::Package;
use lcc_core::settings::{Settings, Theme};
use lcc_core::system::SystemInfo;
use lcc_core::{APP_ID, APP_NAME};
use relm4::adw::prelude::*;
use relm4::{
    Component, ComponentController, ComponentParts, ComponentSender, Controller, SimpleComponent,
    adw, gtk,
};

use crate::pages::Page;
use crate::pages::dashboard::{Dashboard, DashboardMsg, DashboardOutput};
use crate::pages::duplicates::{DupMsg, DupOutput, DuplicatesPage};
use crate::pages::junk::{JunkMsg, JunkOutput, JunkPage};
use crate::pages::leftovers::{LeftoversMsg, LeftoversOutput, LeftoversPage};
use crate::pages::settings::{SettingsOutput, SettingsPage};
use crate::pages::software::{SoftwareMsg, SoftwareOutput, SoftwarePage};
use crate::pages::storage::{StorageMsg, StorageOutput, StoragePage};
use crate::pages::usage::{UsageMsg, UsageOutput, UsagePage};

pub struct App {
    current: Page,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    dashboard: Controller<Dashboard>,
    junk: Controller<JunkPage>,
    leftovers: Controller<LeftoversPage>,
    software: Controller<SoftwarePage>,
    duplicates: Controller<DuplicatesPage>,
    usage: Controller<UsagePage>,
    storage: Controller<StoragePage>,
    /// Whether the usage tracker is meant to be on (from Settings).
    track_usage: bool,
    sender: ComponentSender<App>,
    _settings_page: Controller<SettingsPage>,
}

#[derive(Debug)]
pub enum AppMsg {
    Navigate(Page),
    /// Open a folder in the Storage Analyzer.
    Analyze(PathBuf),
    ShowAbout,
    SettingsChanged(Settings),
    Toast(String),
    /// Junk was cleaned; refresh sizes shown elsewhere.
    JunkCleaned,
    /// Software was uninstalled; refresh lists that show it.
    SoftwareChanged,
    /// Uninstall a package (asked from the App Usage screen).
    Uninstall(Package),
}

pub struct AppWidgets {
    stack: gtk::Stack,
    content_page: adw::NavigationPage,
    sidebar_list: gtk::ListBox,
    split_view: adw::NavigationSplitView,
}

impl SimpleComponent for App {
    type Init = ();
    type Input = AppMsg;
    type Output = ();
    type Root = adw::ApplicationWindow;
    type Widgets = AppWidgets;

    fn init_root() -> Self::Root {
        adw::ApplicationWindow::builder()
            .title(APP_NAME)
            .default_width(1100)
            .default_height(720)
            .width_request(360)
            .height_request(480)
            .build()
    }

    fn init(
        _: Self::Init,
        window: Self::Root,
        sender: ComponentSender<Self>,
    ) -> ComponentParts<Self> {
        let settings = Settings::load();
        apply_theme(settings.theme);
        add_icon_paths(&window);
        let system = SystemInfo::detect();

        let dashboard = Dashboard::builder()
            .launch(system)
            .forward(sender.input_sender(), |out| match out {
                DashboardOutput::Analyze(path) => AppMsg::Analyze(path),
            });
        let junk =
            JunkPage::builder()
                .launch(settings.clone())
                .forward(sender.input_sender(), |out| match out {
                    JunkOutput::Cleaned => AppMsg::JunkCleaned,
                });
        let leftovers = LeftoversPage::builder().launch(settings.clone()).forward(
            sender.input_sender(),
            |out| match out {
                LeftoversOutput::Toast(text) => AppMsg::Toast(text),
            },
        );
        let software = SoftwarePage::builder().launch(settings.clone()).forward(
            sender.input_sender(),
            |out| match out {
                SoftwareOutput::Toast(text) => AppMsg::Toast(text),
                SoftwareOutput::Changed => AppMsg::SoftwareChanged,
            },
        );
        let usage =
            UsagePage::builder()
                .launch(settings.clone())
                .forward(sender.input_sender(), |out| match out {
                    UsageOutput::OpenSettings => AppMsg::Navigate(Page::Settings),
                    UsageOutput::Uninstall(pkg) => AppMsg::Uninstall(*pkg),
                });
        let track_usage = settings.track_app_usage;
        let duplicates = DuplicatesPage::builder().launch(settings.clone()).forward(
            sender.input_sender(),
            |out| match out {
                DupOutput::Toast(text) => AppMsg::Toast(text),
            },
        );
        let storage =
            StoragePage::builder()
                .launch(settings.clone())
                .forward(sender.input_sender(), |out| match out {
                    StorageOutput::Toast(text) => AppMsg::Toast(text),
                });
        let settings_page = SettingsPage::builder().launch(settings).forward(
            sender.input_sender(),
            |out| match out {
                SettingsOutput::Changed(s) => AppMsg::SettingsChanged(s),
                SettingsOutput::Error(e) => AppMsg::Toast(e),
            },
        );

        // Content: one stack child per sidebar entry.
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vhomogeneous(false)
            .hhomogeneous(false)
            .vexpand(true)
            .build();
        for page in Page::ALL {
            let child: gtk::Widget = match page {
                Page::Dashboard => dashboard.widget().clone().upcast(),
                Page::JunkCleaner => junk.widget().clone().upcast(),
                Page::Leftovers => leftovers.widget().clone().upcast(),
                Page::Software => software.widget().clone().upcast(),
                Page::Duplicates => duplicates.widget().clone().upcast(),
                Page::StorageAnalyzer => storage.widget().clone().upcast(),
                Page::AppUsage => usage.widget().clone().upcast(),
                Page::Settings => settings_page.widget().clone().upcast(),
            };
            stack.add_named(&child, Some(page.id()));
        }

        let content_toolbar = adw::ToolbarView::new();
        content_toolbar.add_top_bar(&adw::HeaderBar::new());
        content_toolbar.set_content(Some(&stack));
        let content_page = adw::NavigationPage::builder()
            .title(Page::Dashboard.title())
            .tag("content")
            .child(&content_toolbar)
            .build();

        // Sidebar.
        let sidebar_list = gtk::ListBox::builder()
            .css_classes(["navigation-sidebar"])
            .build();
        for page in Page::ALL {
            sidebar_list.append(&sidebar_row(page));
        }

        let about_button = gtk::Button::builder()
            .icon_name("help-about-symbolic")
            .tooltip_text(format!("About {APP_NAME}"))
            .build();
        {
            let sender = sender.clone();
            about_button.connect_clicked(move |_| sender.input(AppMsg::ShowAbout));
        }
        let sidebar_header = adw::HeaderBar::new();
        sidebar_header.pack_end(&about_button);

        let sidebar_toolbar = adw::ToolbarView::new();
        sidebar_toolbar.add_top_bar(&sidebar_header);
        sidebar_toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(&sidebar_list)
                .build(),
        ));
        let sidebar_page = adw::NavigationPage::builder()
            .title(APP_NAME)
            .tag("sidebar")
            .child(&sidebar_toolbar)
            .build();

        let split_view = adw::NavigationSplitView::builder()
            .sidebar(&sidebar_page)
            .content(&content_page)
            .min_sidebar_width(220.0)
            .max_sidebar_width(280.0)
            .build();

        // Select the first row before connecting, so startup doesn't jump to
        // the content page on narrow windows.
        sidebar_list.select_row(sidebar_list.row_at_index(0).as_ref());
        {
            let sender = sender.clone();
            sidebar_list.connect_row_selected(move |_, row| {
                if let Some(page) = row.and_then(|r| Page::ALL.get(r.index() as usize)) {
                    sender.input(AppMsg::Navigate(*page));
                }
            });
        }
        {
            let split_view = split_view.clone();
            sidebar_list.connect_row_activated(move |_, _| split_view.set_show_content(true));
        }

        // Collapse into one column on narrow windows.
        let breakpoint = adw::Breakpoint::new(
            adw::BreakpointCondition::parse("max-width: 640sp").expect("valid breakpoint"),
        );
        breakpoint.add_setter(&split_view, "collapsed", Some(&true.to_value()));
        window.add_breakpoint(breakpoint);

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&split_view));
        window.set_content(Some(&toasts));

        let model = App {
            current: Page::Dashboard,
            window: window.clone(),
            toasts,
            dashboard,
            junk,
            leftovers,
            software,
            duplicates,
            usage,
            track_usage,
            sender: sender.clone(),
            storage,
            _settings_page: settings_page,
        };
        let widgets = AppWidgets {
            stack,
            content_page,
            sidebar_list,
            split_view,
        };
        ComponentParts { model, widgets }
    }

    fn update(&mut self, msg: Self::Input, _sender: ComponentSender<Self>) {
        match msg {
            AppMsg::Navigate(page) => self.current = page,
            AppMsg::Analyze(path) => {
                self.current = Page::StorageAnalyzer;
                self.storage.emit(StorageMsg::Scan(path));
            }
            AppMsg::ShowAbout => show_about(&self.window),
            AppMsg::SettingsChanged(settings) => {
                apply_theme(settings.theme);
                self.junk.emit(JunkMsg::SettingsChanged(settings.clone()));
                self.leftovers
                    .emit(LeftoversMsg::SettingsChanged(settings.clone()));
                self.software
                    .emit(SoftwareMsg::SettingsChanged(settings.clone()));
                self.duplicates
                    .emit(DupMsg::SettingsChanged(settings.clone()));
                self.usage.emit(UsageMsg::SettingsChanged(settings.clone()));
                if settings.track_app_usage != self.track_usage {
                    self.track_usage = settings.track_app_usage;
                    set_tracker(settings.track_app_usage, self.sender.clone());
                }
                self.storage.emit(StorageMsg::SettingsChanged(settings));
            }
            AppMsg::Toast(text) => {
                // Toasts are markup by default; file names may contain `&` or `<`.
                let toast = adw::Toast::new(&text);
                toast.set_use_markup(false);
                self.toasts.add_toast(toast);
            }
            AppMsg::JunkCleaned => self.dashboard.emit(DashboardMsg::Refresh),
            AppMsg::SoftwareChanged => {
                self.dashboard.emit(DashboardMsg::Refresh);
                self.usage.emit(UsageMsg::Reload);
            }
            AppMsg::Uninstall(pkg) => self.software.emit(SoftwareMsg::AskUninstall(pkg)),
        }
    }

    fn update_view(&self, widgets: &mut Self::Widgets, _sender: ComponentSender<Self>) {
        if widgets.stack.visible_child_name().as_deref() != Some(self.current.id()) {
            widgets.stack.set_visible_child_name(self.current.id());
            widgets.content_page.set_title(self.current.title());
            // Keep the sidebar in sync when a page is opened from elsewhere
            // (e.g. clicking a disk on the Dashboard).
            let index = Page::ALL
                .iter()
                .position(|p| *p == self.current)
                .unwrap_or(0);
            if let Some(row) = widgets.sidebar_list.row_at_index(index as i32)
                && !row.is_selected()
            {
                widgets.sidebar_list.select_row(Some(&row));
            }
            widgets.split_view.set_show_content(true);
        }
    }
}

/// Installs or removes the background usage tracker, off the main thread.
fn set_tracker(enabled: bool, sender: ComponentSender<App>) {
    std::thread::spawn(move || {
        let text = match lcc_core::usage::set_tracker_enabled(enabled) {
            Ok(()) if enabled => {
                "App usage tracking is on. Data stays on this computer.".to_owned()
            }
            Ok(()) => "App usage tracking is off.".to_owned(),
            Err(e) => format!("Couldn't change app usage tracking: {e}"),
        };
        sender.input(AppMsg::Toast(text));
    });
}

fn sidebar_row(page: Page) -> gtk::ListBoxRow {
    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    row_box.append(&gtk::Image::from_icon_name(page.icon()));
    row_box.append(
        &gtk::Label::builder()
            .label(page.title())
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    gtk::ListBoxRow::builder().child(&row_box).build()
}

/// Lets the app find its own icon when it isn't installed system-wide:
/// inside an AppImage (`usr/share/icons` next to `usr/bin`) and in a
/// development build (the repository's `data/icons`).
fn add_icon_paths(window: &adw::ApplicationWindow) {
    let theme = gtk::IconTheme::for_display(&gtk::prelude::WidgetExt::display(window));
    let mut paths = vec![std::path::PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../data/icons"
    ))];
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(|d| d.to_path_buf()))
    {
        paths.push(dir.join("../share/icons"));
    }
    for p in paths.into_iter().filter(|p| p.is_dir()) {
        theme.add_search_path(p);
    }
    gtk::Window::set_default_icon_name(APP_ID);
}

fn apply_theme(theme: Theme) {
    let scheme = match theme {
        Theme::System => adw::ColorScheme::Default,
        Theme::Light => adw::ColorScheme::ForceLight,
        Theme::Dark => adw::ColorScheme::ForceDark,
    };
    adw::StyleManager::default().set_color_scheme(scheme);
}

fn show_about(window: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name(APP_NAME)
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .comments(
            "A friendly cleaner for Linux: find what's filling your disk and clean it safely.",
        )
        .license_type(gtk::License::Gpl30)
        .developer_name("Linux Crap Cleaner contributors")
        .build();
    about.present(Some(window));
}
