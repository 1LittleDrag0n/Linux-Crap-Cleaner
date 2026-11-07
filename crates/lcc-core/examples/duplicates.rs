//! Finds duplicates, big files and empty folders without removing anything:
//! `cargo run --release -p lcc-core --example duplicates -- ~/`
use lcc_core::duplicates::{FindOptions, FindProgress, find, group_name};
use lcc_core::format::{count, duration, human_size};
use lcc_core::safety::PathGuard;

fn main() {
    let root =
        std::fs::canonicalize(std::env::args().nth(1).unwrap_or_else(|| ".".into())).unwrap();
    let home = dirs::home_dir().unwrap();
    let options = FindOptions {
        roots: vec![root],
        ..FindOptions::default()
    };
    let r = find(
        &options,
        &PathGuard::new(&home, &[]),
        &FindProgress::default(),
    )
    .unwrap();
    let wasted: u64 = r.duplicates.iter().map(|g| g.wasted()).sum();
    println!(
        "{} files checked in {}: {} duplicate groups ({} could be freed), {} big files, {} empty folders",
        count(r.files_checked),
        duration(r.elapsed),
        r.duplicates.len(),
        human_size(wasted),
        r.big_files.len(),
        r.empty_folders.len()
    );
    for g in r.duplicates.iter().take(8) {
        println!(
            "  {:>9} wasted  {} × {}  {}",
            human_size(g.wasted()),
            g.files.len(),
            human_size(g.size),
            group_name(g)
        );
        for f in &g.files {
            println!("               {}", f.path.display());
        }
    }
    for f in r.big_files.iter().take(5) {
        println!("  big {:>9}  {}", human_size(f.size), f.path.display());
    }
    for p in r.empty_folders.iter().take(8) {
        println!("  empty  {}", p.display());
    }
}
