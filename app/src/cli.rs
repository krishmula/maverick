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

/// Chrome through Maverick's own link, for a command run while the app is closed.
/// Waits up to a minute for the extension, which tries to connect every 30 seconds.
async fn chrome() -> Result<std::sync::Arc<maverick_core::bridge::Bridge>, String> {
    use maverick_core::bridge::{self, Bridge};
    let socket = maverick_core::socket_path().ok_or("this system has no application data directory")?;
    let listener = bridge::bind(&socket).map_err(|error| match error.kind() {
        std::io::ErrorKind::AddrInUse => {
            "Maverick is open and holds the link to Chrome. Use its Outreach view, or quit it first.".to_owned()
        }
        _ => format!("could not open the link to Chrome: {error}"),
    })?;
    let bridge = Bridge::new();
    tokio::spawn(bridge.clone().serve(listener));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    println!("Waiting for Chrome to connect...");
    while bridge.connections().is_empty() {
        if tokio::time::Instant::now() > deadline {
            return Err("Chrome did not connect within a minute. Is the Maverick Bridge extension loaded?".into());
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    Ok(bridge)
}

/// `maverick linkedin <connections|check|sweep|drafts|type|status>`: outreach on LinkedIn.
/// `page` and `read` are for fixing the parsers when LinkedIn changes its pages: `page` saves one
/// page as Maverick's tab sees it, and `read` shows what Maverick reads in a saved page.
/// Nothing here sends a message or a connection request.
pub fn linkedin(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    use maverick_core::jobs::Outreach;
    use maverick_core::linkedin::{Reader, people_page, posts_page, read_connections};
    use maverick_core::outreach;

    const USAGE: &str = "usage: maverick linkedin connections <Connections.csv> | check [--no-chrome] [--posting <link>]... | sweep | drafts \
                         | credits <profile address> | draft-post <post link> | type <draft id> | redraft <draft id> | status <draft id> <not-started|skipped|drafted|typed|sent|replied> \
                         | page <LinkedIn address> <out.html> | read <people|posts> <saved.html> \
                         | dump-tab <out.html> [script]";
    let database = maverick_core::database_path().ok_or("this system has no application data directory")?;
    let words: Vec<&str> = arguments.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["connections", file] => {
            let text = fs::read_to_string(file)?;
            let connections = read_connections(&text)?;
            if let Some(folder) = database.parent() {
                fs::create_dir_all(folder)?;
            }
            let stored = Store::open(&database)?.replace_connections(&connections)?;
            let without = connections.iter().filter(|c| c.profile_url.is_none()).count();
            println!("Stored {stored} connections in {}.", database.display());
            if without > 0 {
                println!("{without} of them have no profile address in the export.");
            }
        }
        ["check", rest @ ..] => {
            let mut use_chrome = true;
            let mut only = Vec::new();
            let mut rest = rest.iter();
            while let Some(word) = rest.next() {
                match *word {
                    "--no-chrome" => use_chrome = false,
                    "--posting" => only.push(rest.next().ok_or(USAGE)?.to_string()),
                    _ => return Err(USAGE.into()),
                }
            }
            let postings = open_store()?.postings()?;
            let run = runtime()?.block_on(async {
                let bridge = if use_chrome { Some(chrome().await?) } else { None };
                outreach::check_postings(&database, bridge, &only).await
            })?;
            for check in &run.checked {
                let posting = postings.iter().find(|posting| posting.link == check.posting_link);
                let named = posting.map_or(check.posting_link.clone(), |posting| {
                    format!("{} | {}", posting.company, posting.title)
                });
                println!("  {named} | {}", check.summary());
                for person in check.people.iter().filter_map(|person| person.profile_url.as_ref()) {
                    println!("      {person}");
                }
                if let Some(note) = &check.note {
                    println!("      note: {note}");
                }
            }
            println!("{}", run.summary());
        }
        ["sweep"] => {
            let profile = profile()?;
            let run = runtime()?.block_on(async { outreach::sweep(&database, &profile, chrome().await?).await })?;
            println!("{}", run.summary());
            println!("See the drafts with: maverick linkedin drafts");
        }
        ["page", address, out] => {
            if !address.starts_with("https://www.linkedin.com/") {
                return Err(format!("{address} is not a LinkedIn address").into());
            }
            // One page, in a background tab, with nothing clicked.
            let html = runtime()?.block_on(async {
                let mut reader = Reader::open(chrome().await?, 1)
                    .await
                    .map_err(|stop| stop.to_string())?;
                let html = reader
                    .load(address, maverick_core::linkedin::First::Nothing)
                    .await
                    .map_err(|stop| stop.to_string());
                reader.close().await;
                html
            })?;
            fs::write(out, html)?;
            println!("Saved {address} to {out}.");
        }
        ["read", "people", file] => {
            let page = people_page(&fs::read_to_string(file)?);
            println!(
                "company: {}",
                page.company.as_deref().unwrap_or("none; not a People page")
            );
            for person in &page.people {
                let degree = person.degree.as_deref().unwrap_or("-");
                println!("  {} | {degree} | {}", person.name, person.title);
            }
            println!("{} people", page.people.len());
        }
        ["read", "posts", file] => {
            let posts = posts_page(&fs::read_to_string(file)?);
            for post in &posts {
                let shown: String = post.text.chars().take(80).collect();
                println!(
                    "  {} | {} | {} | {} chars{}\n      {}\n      {}",
                    post.poster,
                    post.degree.as_deref().unwrap_or("-"),
                    post.headline,
                    post.text.chars().count(),
                    if post.complete { "" } else { ", cut short" },
                    post.link(),
                    shown.replace('\n', " ")
                );
            }
            println!("{} posts", posts.len());
        }
        // Saves the open LinkedIn profile tab as it is now, dialogs included, or what `script` gives
        // there. It reads only, so a stuck typing step can be fixed from the real page.
        ["dump-tab", out, rest @ ..] => {
            let html = runtime()?.block_on(async {
                let bridge = chrome().await?;
                let profile_tab = || match bridge.state() {
                    maverick_core::bridge::BrowserState::Connected { tabs } => {
                        let mut profiles: Vec<_> =
                            tabs.into_iter().filter(|tab| tab.tab.url.contains("linkedin.com/in/")).collect();
                        profiles.sort_by_key(|tab| !tab.tab.active);
                        profiles.into_iter().next()
                    }
                    _ => None,
                };
                let mut tab = None;
                for _ in 0..50 {
                    tab = profile_tab();
                    if tab.is_some() {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                let tab = tab.ok_or_else(|| {
                    let open = match bridge.state() {
                        maverick_core::bridge::BrowserState::Connected { tabs } => tabs
                            .iter()
                            .map(|tab| format!("{:?}:{}", tab.connection, tab.tab.url.split('?').next().unwrap_or_default()))
                            .collect::<Vec<_>>()
                            .join(", "),
                        _ => String::new(),
                    };
                    format!("no LinkedIn profile tab is open ({open} tabs seen)")
                })?;
                let page = maverick_core::page::Page::attach(bridge, &tab).await.map_err(|e| e.to_string())?;
                let script = r#"(() => { const out = [document.documentElement.outerHTML];
                    for (const el of document.querySelectorAll('*')) if (el.shadowRoot) out.push('<!--SHADOW ' + el.tagName + '-->' + el.shadowRoot.innerHTML);
                    for (const f of document.querySelectorAll('iframe')) { try { out.push('<!--IFRAME-->' + f.contentDocument.documentElement.outerHTML); } catch (e) { out.push('<!--IFRAME blocked ' + f.src + '-->'); } }
                    return out.join('\n'); })()"#;
                let script = rest.first().copied().unwrap_or(script);
                let html = page.evaluate(script).await.map_err(|e| e.to_string());
                let _ = page.detach().await;
                html.map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
            })?;
            fs::write(out, html)?;
        }
        ["drafts"] => {
            for draft in open_store()?.drafts()? {
                println!(
                    "#{} {} {} to {} ({}) | {}",
                    draft.id,
                    draft.status.as_str(),
                    draft.slot.as_str(),
                    draft.person,
                    draft.profile_url,
                    draft.about.link()
                );
                if let Some(subject) = &draft.subject {
                    println!("    subject: {subject}");
                }
                println!("    {}", draft.text.replace('\n', "\n    "));
            }
        }
        ["redraft", id] => {
            let id: i64 = id.parse().map_err(|_| USAGE)?;
            let profile = profile()?;
            let draft = runtime()?.block_on(outreach::redraft(&database, &profile, id))?;
            println!("#{} is now a {}.", draft.id, draft.slot.as_str());
            if let Some(subject) = &draft.subject {
                println!("    subject: {subject}");
                println!("    Sending an InMail uses one of your InMail credits.");
            }
            println!("    {}", draft.text.replace('\n', "\n    "));
        }
        ["type", id] => {
            let id: i64 = id.parse().map_err(|_| USAGE)?;
            let resume = maverick_core::answers_path()
                .and_then(|file| Answers::load(&file).ok())
                .map(|answers| answers.resume);
            let typed = runtime()?
                .block_on(async { outreach::type_approved(&database, chrome().await?, id, resume.as_deref()).await })?;
            println!("Draft #{id}: {}", typed.said());
            println!("Then: maverick linkedin status {id} sent");
        }
        // Shows the message window of a profile outside the network, where LinkedIn says how many
        // InMail credits are left. It types nothing.
        ["credits", profile] => {
            let text = runtime()?.block_on(async {
                let mut reader = Reader::open(chrome().await?, 1)
                    .await
                    .map_err(|stop| stop.to_string())?;
                let text = reader.message_window(profile).await.map_err(|stop| stop.to_string());
                reader.close().await;
                text
            })?;
            println!("{text}");
            match maverick_core::linkedin::inmail_cost(&text) {
                Some(cost) => println!("An InMail to this person: {cost:?}"),
                None => println!("The window says nothing about what an InMail costs."),
            }
        }
        // Drafts a message to the author of a hiring post the store holds, as the Posts tab's draft button does.
        ["draft-post", link] => {
            let profile = profile()?;
            let made = runtime()?
                .block_on(async { outreach::draft_for_post(&database, &profile, Some(chrome().await?), link).await })?;
            let draft = made.draft;
            println!(
                "#{} {} to {} ({})",
                draft.id,
                draft.slot.as_str(),
                draft.person,
                draft.profile_url
            );
            if let Some(why) = made.credits_unread {
                println!("    Your InMail credits could not be read ({why}), so this is a connection note.");
            }
            if let Some(subject) = &draft.subject {
                println!("    subject: {subject}");
            }
            println!("    {}", draft.text.replace('\n', "\n    "));
        }
        ["status", id, status] => {
            let id: i64 = id.parse().map_err(|_| USAGE)?;
            let status = Outreach::parse(&status.replace('-', "_")).ok_or(USAGE)?;
            if status == Outreach::Drafted {
                // Back to waiting for approval, as the Reset button does: only the draft changes.
                Store::open(&database)?.reset_draft(id, jiff::Timestamp::now())?;
            } else {
                Store::open(&database)?.set_draft_status(id, status, jiff::Timestamp::now())?;
            }
            println!("Draft #{id} is now {}.", status.as_str().replace('_', " "));
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}
