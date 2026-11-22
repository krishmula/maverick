//! Commands that run in the terminal and open no window.

use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;

use maverick_core::answers::Answers;
use maverick_core::description::Source;
use maverick_core::jobs::{Posting, Stage, posting_address};
use maverick_core::rules::{SeenJobs, prefilter};
use maverick_core::score::{self, APPLY_READY, Profile};
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

fn open_store() -> Result<Store, Box<dyn Error>> {
    let database = maverick_core::database_path().ok_or("this system has no application data directory")?;
    Ok(Store::open(&database)?)
}

fn profile() -> Result<Profile, Box<dyn Error>> {
    let file = maverick_core::answers_path().ok_or("this system has no application data directory")?;
    let answers = Answers::load(&file)?;
    if answers.background.is_empty() {
        return Err(format!(
            "{} has no background, so there is no resume to score against",
            file.display()
        )
        .into());
    }
    Ok(Profile::from(&answers))
}

fn runtime() -> std::io::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()
}

/// One line about a scored posting, for the terminal.
fn scored_line(posting: &Posting) -> String {
    let score = posting.score.map_or("--".to_owned(), |score| score.to_string());
    let notes = posting.notes.as_deref().unwrap_or("");
    format!(
        "  {score:>3} {:8} {} | {} | {notes}",
        posting.stage.as_str(),
        posting.company,
        posting.title
    )
}

/// The scoring run: marks repeats and rule skips, then reads and scores up to `limit` found postings.
/// With `dry_run`, nothing is written.
pub fn score(limit: usize, dry_run: bool) -> Result<(), Box<dyn Error>> {
    let mut store = open_store()?;
    let profile = profile()?;
    let plan = score::plan(&store.postings()?, &store.applications()?, limit);
    println!(
        "{} repeats, {} skipped by the title rules, {} to score, {} left for the next run, {} with no readable source",
        plan.duplicates.len(),
        plan.skipped.len(),
        plan.to_score.len(),
        plan.waiting,
        plan.unreadable
    );
    if !dry_run {
        store.save_decisions(&plan.duplicates)?;
        store.save_decisions(&plan.skipped)?;
    }
    let scored = runtime()?.block_on(score::score_all(plan.to_score, profile, |posting| {
        println!("{}", scored_line(posting));
    }));
    if !dry_run {
        store.save_decisions(&scored)?;
    }
    let ready = scored
        .iter()
        .filter(|p| p.notes.as_deref().is_some_and(|n| n.starts_with(APPLY_READY)))
        .count();
    let review = scored.iter().filter(|p| p.stage == Stage::Review).count() - ready;
    let skipped = scored.iter().filter(|p| p.stage == Stage::Skipped).count();
    let failed = scored.iter().filter(|p| p.stage == Stage::Found).count();
    println!("Scored: {ready} apply-ready, {review} to review, {skipped} skipped, {failed} not scored yet");
    if dry_run {
        println!("Dry run: nothing was written.");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Bucket {
    Ready,
    Review,
    Skip,
}

fn bucket(posting: &Posting) -> Option<Bucket> {
    match posting.stage {
        Stage::Review if posting.notes.as_deref().is_some_and(|n| n.starts_with("Apply-ready")) => Some(Bucket::Ready),
        Stage::Review | Stage::Applied => Some(Bucket::Review),
        Stage::Skipped => Some(Bucket::Skip),
        _ => None,
    }
}

/// Scores again `count` postings the earlier funnel already scored, writes nothing,
/// and compares the two.
pub fn check_scores(count: usize) -> Result<(), Box<dyn Error>> {
    let store = open_store()?;
    let profile = profile()?;
    let mut earlier: Vec<Posting> = store
        .postings()?
        .into_iter()
        .filter(|posting| posting.score.is_some() && bucket(posting).is_some())
        .filter(|posting| Source::of(&posting_address(&posting.link)).is_some())
        .collect();
    earlier.reverse();
    earlier.truncate(count);
    let before: Vec<(String, u8, Bucket)> = earlier
        .iter()
        .filter_map(|p| Some((p.link.clone(), p.score?, bucket(p)?)))
        .collect();
    let scored = runtime()?.block_on(score::score_all(earlier, profile, |_| {}));

    let (mut compared, mut same_bucket, mut total_gap, mut closed, mut failed) = (0, 0, 0u32, 0, 0);
    println!("earlier now  earlier-bucket now-bucket | company | title | now-notes");
    for (link, was, was_bucket) in &before {
        let Some(now) = scored.iter().find(|p| &p.link == link) else {
            continue;
        };
        if now.flags.as_deref() == Some("Blocking: posting closed") {
            closed += 1;
            continue;
        }
        let (Some(score), Some(now_bucket)) = (now.score, bucket(now)) else {
            failed += 1;
            println!(
                "  {was:>3}  --  {:?} | {} | {} | {}",
                was_bucket,
                now.company,
                now.title,
                now.notes.as_deref().unwrap_or("")
            );
            continue;
        };
        compared += 1;
        same_bucket += u32::from(*was_bucket == now_bucket);
        total_gap += u32::from(was.abs_diff(score));
        println!(
            "  {was:>3} {score:>3}  {:?} {:?} | {} | {} | {}",
            was_bucket,
            now_bucket,
            now.company,
            now.title,
            now.notes.as_deref().unwrap_or("")
        );
    }
    println!(
        "Compared {compared}: same decision on {same_bucket}, mean score gap {:.1}",
        f64::from(total_gap) / f64::from(compared.max(1))
    );
    println!("Closed since: {closed}. Could not score: {failed}.");
    Ok(())
}
