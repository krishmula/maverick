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

/// One funnel pass now, whatever the hour: pulls new postings, then scores what is found.
pub fn run() -> Result<(), Box<dyn Error>> {
    let database = maverick_core::database_path().ok_or("this system has no application data directory")?;
    let profile = profile()?;
    let pass = runtime()?.block_on(async {
        let sheet = maverick_core::sheet::linked_session().await?;
        if let Some(session) = &sheet {
            println!("Reading and writing the tracker sheet {:?}.", session.sheet.name);
        }
        maverick_core::funnel::run_once(&database, profile, sheet).await
    })?;
    for posting in &pass.scored {
        println!("{}", scored_line(posting));
    }
    println!(
        "{} repeats and {} title-rule skips marked; {} with no readable source",
        pass.duplicates, pass.skipped_by_rules, pass.unreadable
    );
    println!("{}", pass.summary());
    Ok(())
}

/// `maverick models <list|check|key|forget|use>`: sets up the models Maverick runs.
pub fn models(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    use maverick_core::credentials::Password;
    use maverick_core::providers::{self, Choice, Choices, Job, Provider, Status};

    const USAGE: &str = "usage: maverick models list | check [provider] | key <provider> < key-file | forget <provider> | use <mav|forms|scoring> <provider> [model]";
    let provider_named = |name: &str| -> Result<Provider, Box<dyn Error>> {
        let wanted = name.to_lowercase().replace([' ', '-'], "");
        Provider::ALL
            .into_iter()
            .find(|provider| provider.name().to_lowercase().replace(' ', "") == wanted)
            .ok_or_else(|| format!("no provider is called {name}; use one of claude-code, codex, opencode, anthropic, openai, openrouter").into())
    };
    let runtime = runtime()?;
    let show = |provider: Provider, status: Status| {
        let (mark, detail) = match status {
            Status::Ready(detail) => ("ready", detail),
            Status::Present(detail) => ("found", detail),
            Status::Missing(detail) => ("missing", format!("{detail}. {}", provider.help())),
            Status::Failed(detail) => ("failed", detail),
        };
        println!("  {:<12} {mark:<8} {detail}", provider.name());
    };
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["list"] | [] => {
            for provider in Provider::ALL {
                show(provider, runtime.block_on(providers::look(provider)));
            }
            let choices = Choices::current();
            for job in Job::ALL {
                println!("  {:<12} uses {}", job.name(), choices.for_job(job).label());
            }
        }
        ["check"] => {
            for provider in Provider::ALL {
                show(provider, runtime.block_on(providers::check(provider, "")));
            }
        }
        ["check", name] => {
            let provider = provider_named(name)?;
            show(provider, runtime.block_on(providers::check(provider, "")));
        }
        ["key", name] => {
            let provider = provider_named(name)?;
            if provider.command().is_some() {
                return Err(format!("{} signs in through its own command, not a key", provider.name()).into());
            }
            let mut key = String::new();
            std::io::stdin().read_line(&mut key)?;
            let key = Password::new(key.trim());
            if key.reveal().is_empty() {
                return Err("give the key on standard input".into());
            }
            runtime.block_on(providers::save_key(provider, &key))?;
            show(provider, runtime.block_on(providers::check(provider, "")));
        }
        ["forget", name] => {
            let provider = provider_named(name)?;
            runtime.block_on(providers::forget_key(provider))?;
            println!("Removed the {} key.", provider.name());
        }
        ["use", job, name, ref model @ ..] => {
            let job = match job {
                "mav" => Job::Mav,
                "forms" => Job::Forms,
                "scoring" => Job::Scoring,
                _ => return Err(USAGE.into()),
            };
            let choice = Choice {
                provider: provider_named(name)?,
                model: model.join(" "),
            };
            let file = providers::choices_path().ok_or("this system has no application data directory")?;
            let mut choices = Choices::load(&file)?;
            choices.set(job, choice.clone());
            choices.save(&file)?;
            println!("{} now uses {}.", job.name(), choice.label());
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// `maverick mav <message>`: shows what Mav would propose. It never writes anything.
pub fn mav(message: &str) -> Result<(), Box<dyn Error>> {
    use maverick_core::mav::{self, Context};

    if message.trim().is_empty() {
        return Err("usage: maverick mav <what to remember or plan>".into());
    }
    let runtime = runtime()?;
    let reply = runtime.block_on(async {
        let context = Context::read(jiff::ZonedDateTime::now()).await?;
        println!(
            "Read {} lists, {} tasks and {} events.",
            context.lists.len(),
            context.agenda.overdue.len() + context.agenda.today.len() + context.agenda.upcoming.len(),
            context.events.len()
        );
        mav::ask(message, &[], &context).await
    })?;
    println!("Mav: {}", reply.text);
    for proposal in reply.proposals {
        match proposal {
            Ok(action) => {
                println!("  [{}] {}", action.kind(), action.title());
                for (label, value) in action.facts() {
                    println!("      {label:<8} {value}");
                }
            }
            Err(why) => println!("  [refused] {why}"),
        }
    }
    Ok(())
}

/// `maverick ticktick <token|today|forget>`: connects TickTick and shows the agenda.
pub fn ticktick(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    use maverick_core::credentials::Password;
    use maverick_core::ticktick::{self, Entry};

    const USAGE: &str = "usage: maverick ticktick token < token-file | today | forget";
    let runtime = runtime()?;
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["token"] => {
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            let token = Password::new(token.trim());
            if token.reveal().is_empty() {
                return Err("give the API token on standard input".into());
            }
            // A token TickTick refuses is not worth keeping.
            let lists = runtime.block_on(ticktick::projects(&token))?;
            runtime.block_on(ticktick::save_token(&token))?;
            println!(
                "Stored the TickTick token in the Keychain. It sees {} lists.",
                lists.len()
            );
        }
        ["today"] => {
            let token = runtime
                .block_on(ticktick::stored_token())?
                .ok_or("no TickTick token is stored yet; run `maverick ticktick token` first")?;
            let now = jiff::ZonedDateTime::now();
            let (lists, tasks) = runtime.block_on(ticktick::open_tasks(&token))?;
            let entries = ticktick::entries(&tasks, &lists, now.time_zone());
            let agenda = ticktick::Agenda::from_entries(&entries, &now);
            let print = |heading: &str, entries: &[Entry]| {
                println!("{heading} ({})", entries.len());
                for entry in entries {
                    println!(
                        "  {:<16} {:<6} {}  [{}]",
                        entry.when(),
                        entry.priority.name(),
                        entry.title,
                        entry.list
                    );
                }
            };
            print("Overdue", &agenda.overdue);
            print("Today", &agenda.today);
            print("Coming week", &agenda.upcoming);
        }
        ["forget"] => {
            runtime.block_on(ticktick::forget_token())?;
            println!("Removed the TickTick token from the Keychain.");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// `maverick google <client|add|list|check|remove>`: manages the connected Google accounts.
pub fn google(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    use maverick_core::google::{self, Client};

    const USAGE: &str = "usage: maverick google client <client id> < secret-file | add | list | check | get <email> <url> | remove <email>";
    let file = google::accounts_path().ok_or("this system has no application data directory")?;
    let runtime = runtime()?;
    let client = || -> Result<Client, Box<dyn Error>> {
        runtime
            .block_on(Client::stored())?
            .ok_or_else(|| "no Google OAuth client is stored yet; run `maverick google client <id>` first".into())
    };
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["client", id] => {
            let mut secret = String::new();
            std::io::stdin().read_line(&mut secret)?;
            let secret = secret.trim();
            if secret.is_empty() {
                return Err("give the client secret on standard input".into());
            }
            runtime.block_on(Client::new(id, maverick_core::credentials::Password::new(secret)).save())?;
            println!("Stored the OAuth client {id} in the Keychain.");
        }
        ["add"] => {
            let client = client()?;
            let account = runtime.block_on(google::connect(&client, &file, |url| {
                println!("Opening the Google sign-in in your browser. If it does not open, visit:\n{url}");
                let _ = std::process::Command::new("/usr/bin/open").arg(url).status();
            }))?;
            println!("Connected {} with {} scopes.", account.email, account.scopes.len());
        }
        ["list"] => {
            let accounts = google::accounts(&file)?;
            if accounts.is_empty() {
                println!("No Google accounts are connected.");
            }
            for account in accounts {
                println!(
                    "{}  connected {}  {}",
                    account.email,
                    account.connected_at,
                    account.scopes.join(" ")
                );
            }
        }
        ["check"] => {
            let client = client()?;
            for account in google::accounts(&file)? {
                let checked = runtime.block_on(async {
                    let token = google::access_token(&client, &account.email).await?;
                    google::call(
                        &token,
                        "GET",
                        "https://www.googleapis.com/drive/v3/about?fields=user(emailAddress)",
                        None,
                    )
                    .await
                });
                match checked {
                    Ok(about) => println!(
                        "{}: works (Drive sees {})",
                        account.email, about["user"]["emailAddress"]
                    ),
                    Err(error) => println!("{}: {error}", account.email),
                }
            }
        }
        ["get", email, url] => {
            let client = client()?;
            let reply = runtime.block_on(async {
                let token = google::access_token(&client, email).await?;
                google::call(&token, "GET", url, None).await
            })?;
            println!("{}", serde_json::to_string_pretty(&reply)?);
        }
        ["remove", email] => {
            runtime.block_on(google::disconnect(&file, email))?;
            println!("Removed {email}.");
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

/// `maverick tracker link [name]` links the live tracker sheet; `maverick tracker` shows the link.
pub fn tracker(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    use maverick_core::google::{self, Client};
    use maverick_core::sheet::{self, TrackerSheet};

    let file = sheet::config_path().ok_or("this system has no application data directory")?;
    match arguments.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => match TrackerSheet::linked(&file)? {
            Some(linked) => println!(
                "Linked: {:?} ({}) through {}",
                linked.name, linked.spreadsheet_id, linked.account
            ),
            None => println!("No tracker sheet is linked. Run `maverick tracker link <part of its name>`."),
        },
        ["link", name] => {
            let runtime = runtime()?;
            let client = runtime
                .block_on(Client::stored())?
                .ok_or("no Google OAuth client is stored yet; run `maverick google client <id>` first")?;
            let accounts = google::accounts(&google::accounts_path().ok_or("no data directory")?)?;
            let found = runtime.block_on(sheet::find(&client, &accounts, name))?;
            let Some(chosen) = found.first() else {
                return Err(format!("no connected account has a spreadsheet named like {name:?}").into());
            };
            chosen.save(&file)?;
            println!("Linked {:?} through {}.", chosen.name, chosen.account);
            for other in &found[1..] {
                println!(
                    "  Also found {:?} through {}; pass a longer name to pick it.",
                    other.name, other.account
                );
            }
        }
        _ => return Err("usage: maverick tracker [link <part of its name>]".into()),
    }
    Ok(())
}
