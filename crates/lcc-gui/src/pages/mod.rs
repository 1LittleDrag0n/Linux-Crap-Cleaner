//! The screens listed in the sidebar.

pub mod dashboard;
pub mod duplicates;
pub mod junk;
pub mod leftovers;
pub mod settings;
pub mod software;
pub mod storage;
pub mod usage;

/// One entry in the sidebar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Dashboard,
    JunkCleaner,
    StorageAnalyzer,
    Leftovers,
    Software,
    AppUsage,
    Duplicates,
    Settings,
}

impl Page {
    /// Sidebar order.
    pub const ALL: [Page; 8] = [
        Page::Dashboard,
        Page::JunkCleaner,
        Page::StorageAnalyzer,
        Page::Leftovers,
        Page::Software,
        Page::AppUsage,
        Page::Duplicates,
        Page::Settings,
    ];

    /// Stable name used as the stack child name.
    pub fn id(self) -> &'static str {
        match self {
            Page::Dashboard => "dashboard",
            Page::JunkCleaner => "junk",
            Page::StorageAnalyzer => "storage",
            Page::Leftovers => "leftovers",
            Page::Software => "software",
            Page::AppUsage => "usage",
            Page::Duplicates => "duplicates",
            Page::Settings => "settings",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::Dashboard => "Dashboard",
            Page::JunkCleaner => "Junk Cleaner",
            Page::StorageAnalyzer => "Storage Analyzer",
            Page::Leftovers => "System Leftovers",
            Page::Software => "Installed Software",
            Page::AppUsage => "App Usage",
            Page::Duplicates => "Duplicates & Big Files",
            Page::Settings => "Settings",
        }
    }

    /// Symbolic icon from the standard icon theme.
    pub fn icon(self) -> &'static str {
        match self {
            Page::Dashboard => "computer-symbolic",
            Page::JunkCleaner => "user-trash-symbolic",
            Page::StorageAnalyzer => "drive-harddisk-symbolic",
            Page::Leftovers => "package-x-generic-symbolic",
            Page::Software => "application-x-executable-symbolic",
            Page::AppUsage => "document-open-recent-symbolic",
            Page::Duplicates => "edit-copy-symbolic",
            Page::Settings => "emblem-system-symbolic",
        }
    }
}
