//! Lists installed software and leftovers without changing anything:
//! `cargo run --release -p lcc-core --example leftovers`
use lcc_core::format::human_size;
use lcc_core::junk::Places;
use lcc_core::leftovers::find_leftovers;
use lcc_core::packages::list_installed;
use lcc_core::safety::PathGuard;

fn main() {
    let t = std::time::Instant::now();
    let pkgs = list_installed();
    let apps = pkgs.iter().filter(|p| p.is_app).count();
    let explicit = pkgs.iter().filter(|p| p.explicit).count();
    println!(
        "{} packages ({apps} apps, {explicit} installed on purpose) in {:?}",
        pkgs.len(),
        t.elapsed()
    );
    let mut biggest: Vec<_> = pkgs.iter().filter(|p| p.is_app).collect();
    biggest.sort_by_key(|p| std::cmp::Reverse(p.size));
    for p in biggest.iter().take(8) {
        println!(
            "  {:>9}  {} ({}, {})",
            human_size(p.size),
            p.display_name,
            p.name,
            p.source.name()
        );
    }

    let places = Places::current().expect("home");
    let t = std::time::Instant::now();
    let found = find_leftovers(&places, &PathGuard::new(&places.home, &[]));
    println!("\n{} leftovers in {:?}", found.len(), t.elapsed());
    for l in &found {
        println!(
            "  {:?} {:>9}  {}  [{}]",
            l.category,
            human_size(l.size),
            l.name,
            l.detail
        );
    }
}
