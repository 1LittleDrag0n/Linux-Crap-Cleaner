//! Shows what the Junk Cleaner would find, without removing anything:
//! `cargo run --release -p lcc-core --example junk`
use lcc_core::format::{count, human_size};
use lcc_core::junk::{
    AnalyzeOptions, Places, Status, analyze, builtin_cleaners, running_processes,
};
use lcc_core::safety::PathGuard;

fn main() {
    let places = Places::current().expect("home folder");
    let guard = PathGuard::new(&places.home, &[]);
    let started = std::time::Instant::now();
    let findings = analyze(
        &builtin_cleaners(),
        &places,
        &guard,
        &running_processes(),
        &AnalyzeOptions::default(),
    );
    for f in &findings {
        let status = match &f.status {
            Status::Ready => "ready".to_owned(),
            Status::Empty => continue,
            Status::AppRunning(app) => format!("skipped: {app} is running"),
            Status::NeedsRoot => "needs administrator".to_owned(),
        };
        println!(
            "{:>10}  {:<28} {:>6} items {:>9} files  {status}",
            human_size(f.size),
            f.cleaner.name,
            f.items.len(),
            count(f.files)
        );
    }
    println!("analyzed in {:?}", started.elapsed());
}
