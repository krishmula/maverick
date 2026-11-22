//! Commands that run in the terminal and open no window.

use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use maverick_core::jobs::Stage;
use maverick_core::rules::{SeenJobs, prefilter};
use maverick_core::store::Store;
use maverick_core::tracker;

fn counts<T: Copy>(counts: &[(T, u32)], name: fn(T) -> &'static str) -> String {
    let parts: Vec<String> = counts
        .iter()
        .map(|&(state, count)| format!("{count} {}", name(state).replace('_', " ")))
        .collect();
    parts.join(", ")
}

/// Copies the tracker spreadsheet into the local database. Safe to repeat.
pub fn import_tracker(file: &Path) -> Result<(), Box<dyn Error>> {
    let tracker = tracker::read(BufReader::new(File::open(file)?))?;
    let database = maverick_core::database_path().ok_or("this system has no application data directory")?;
    if let Some(folder) = database.parent() {
        fs::create_dir_all(folder)?;
    }
    let mut store = Store::open(&database)?;
    store.save_tracker(&tracker)?;
    let summary = store.summary()?;

    println!("Imported {} into {}", file.display(), database.display());
    println!("The database now holds:");
    println!("  Postings: {}", counts(&summary.postings, |stage| stage.as_str()));
    println!(
        "  Applications: {}",
        counts(&summary.applications, |status| status.as_str())
    );
    if summary.applications_without_role > 0 {
        println!(
            "    {} of them have no company or role in the file.",
            summary.applications_without_role
        );
    }
    println!(
        "  Hiring posts: {}",
        counts(&summary.hiring_posts, |outreach| outreach.as_str())
    );
    if !tracker.unreadable.is_empty() {
        let rows = match tracker.unreadable.len() {
            1 => "1 row".to_owned(),
            count => format!("{count} rows"),
        };
        println!("Left out {rows} that could not be read:");
        for unreadable in &tracker.unreadable {
            println!("  {} row {}: {}", unreadable.sheet, unreadable.row, unreadable.problem);
        }
    }
    Ok(())
}

/// Compares the code rules with the decisions already stored for each posting.
///
/// The stored decisions came from the earlier funnel, where a model judged every row.
/// The report shows what the rules would skip, and where the two disagree.
pub fn check_rules() -> Result<(), Box<dyn Error>> {
    let database = maverick_core::database_path().ok_or("this system has no application data directory")?;
    let store = Store::open(&database)?;
    let postings = store.postings()?;

    let mut skipped_unscored = 0;
    let mut caught = 0;
    let mut low_scored = 0;
    let mut disagreements = Vec::new();
    for posting in &postings {
        let reason = prefilter(&posting.title, posting.location.as_deref());
        match (posting.stage, posting.score) {
            (Stage::Skipped, None) => {
                skipped_unscored += 1;
                caught += u32::from(reason.is_some());
            }
            (_, Some(score)) => match reason {
                Some(reason) if score >= 60 => disagreements.push((score, posting, reason)),
                Some(_) => low_scored += 1,
                None => {}
            },
            _ => {}
        }
    }
    println!("Title and location rules");
    println!("  They skip {caught} of the {skipped_unscored} postings that were skipped without a score.");
    println!("  They skip {low_scored} postings that were scored under 60.");
    println!(
        "  They skip {} postings that were scored 60 or more:",
        disagreements.len()
    );
    disagreements.sort_by_key(|(score, ..)| std::cmp::Reverse(*score));
    for (score, posting, reason) in &disagreements {
        println!("    {score} {:8} {} ({reason})", posting.stage.as_str(), posting.title);
    }

    // A posting that was applied to matches its own application, so that match is not a repeat.
    let mut applied_to = SeenJobs::default();
    for role in store.applications()?.iter().filter_map(|sent| sent.role.as_deref()) {
        applied_to.insert(role, "");
    }
    let mut seen = SeenJobs::default();
    let mut both = 0;
    let (mut only_rule, mut only_stored) = (Vec::new(), Vec::new());
    for posting in &postings {
        let repeat = seen.insert(&posting.company, &posting.title);
        let applied = posting.stage != Stage::Applied && applied_to.insert(&posting.company, &posting.title);
        match (repeat || applied, posting.stage == Stage::Duplicate) {
            (true, true) => both += 1,
            (true, false) => only_rule.push(posting),
            (false, true) => only_stored.push(posting),
            (false, false) => {}
        }
    }
    println!("Duplicate rule");
    println!("  Both mark {both} postings as duplicates.");
    for (label, postings) in [
        ("Only the rule marks", only_rule),
        ("Only the stored decision marks", only_stored),
    ] {
        println!("  {label} {}:", postings.len());
        for posting in postings {
            println!(
                "    {:8} {} | {}",
                posting.stage.as_str(),
                posting.company,
                posting.title
            );
        }
    }
    Ok(())
}
