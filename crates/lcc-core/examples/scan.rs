//! Scans a folder and prints the biggest items: `cargo run --release -p lcc-core --example scan -- ~/`
use lcc_core::format::{count, duration, human_size};
use lcc_core::scan::{ScanOptions, ScanProgress, scan};

fn main() {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let root = std::fs::canonicalize(&root).expect("folder exists");
    let r = scan(&root, &ScanOptions::default(), &ScanProgress::default()).expect("scan");
    println!(
        "{}: {} in {} files, {} folders ({} unreadable) in {}",
        root.display(),
        human_size(r.root.size),
        count(r.root.file_count),
        count(r.root.folder_count),
        r.unreadable_folders,
        duration(r.elapsed)
    );
    for e in r.root.entries().iter().take(10) {
        println!("  {:>10}  {}", human_size(e.size), e.name.to_string_lossy());
    }
}
