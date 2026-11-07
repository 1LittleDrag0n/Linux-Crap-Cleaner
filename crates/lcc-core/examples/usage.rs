//! Prints when each app was last used, without changing anything:
//! `cargo run --release -p lcc-core --example usage`
use lcc_core::format::{ago, human_size, now};
use lcc_core::junk::Places;
use lcc_core::packages::list_installed;
use lcc_core::usage::{Basis, UsageDb, app_usage};

fn main() {
    let mut usage = app_usage(
        list_installed(),
        &UsageDb::load(),
        &Places::current().unwrap(),
    );
    usage.sort_by_key(|u| u.last_used.unwrap_or(0));
    let now = now();
    let unknown = usage.iter().filter(|u| u.basis == Basis::Unknown).count();
    println!(
        "{} apps, {} with no usage information",
        usage.len(),
        unknown
    );
    for u in usage.iter().filter(|u| u.basis != Basis::Unknown) {
        println!(
            "  {:<16} {:>9}  {}",
            ago(u.last_used.unwrap(), now),
            human_size(u.package.size),
            u.package.display_name
        );
    }
}
