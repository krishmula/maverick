//! Finds new postings without email: Simplify publishes its new-grad list as JSON on GitHub.
//! The SWE List digest is built from the same list, a day later.

use std::collections::HashSet;

use jiff::{SignedDuration, Timestamp};
use serde::Deserialize;

use crate::apply;
use crate::description;
use crate::jobs::{Posting, Stage, posting_address};

pub const SIMPLIFY_NEW_GRAD: &str =
    "https://raw.githubusercontent.com/SimplifyJobs/New-Grad-Positions/dev/.github/scripts/listings.json";

/// The `source` of a posting this module found.
pub const SOURCE: &str = "Simplify";

/// How far back a pull looks. Older listings were seen by an earlier pull or by the SWE List email.
const WINDOW: SignedDuration = SignedDuration::from_hours(72);

/// The list's categories that hold software jobs. Hardware, Quant and Product are left out.
const CATEGORIES: &[&str] = &[
    "Software",
    "Software Engineering",
    "AI/ML/Data",
    "Data Science, AI & Machine Learning",
];

#[derive(Deserialize)]
struct Listing {
    id: String,
    company_name: String,
    title: String,
    url: String,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    locations: Vec<String>,
    active: bool,
    #[serde(default = "visible")]
    is_visible: bool,
    date_posted: i64,
}

fn visible() -> bool {
    true
}

/// The Simplify page of a posting, which is also how a SWE List email links to it.
fn simplify_link(id: &str) -> String {
    format!("https://simplify.jobs/p/{id}")
}

/// The Simplify id in a link to a Simplify page, with or without a slug and campaign tags.
fn simplify_id(link: &str) -> Option<&str> {
    let rest = link.strip_prefix("https://simplify.jobs/p/")?;
    rest.split(['/', '?', '#']).next().filter(|id| !id.is_empty())
}

/// The address used to spot the same posting arriving by two routes.
fn comparable(address: &str) -> String {
    apply::posting_address(address).unwrap_or_else(|| address.split(['?', '#']).next().unwrap_or("").to_owned())
}

/// The listings in `list` that are open, software jobs, posted in the window before `now`,
/// and not yet in `known`, as found postings.
pub fn new_postings(list: &str, known: &[Posting], now: Timestamp) -> Result<Vec<Posting>, String> {
    let listings: Vec<Listing> =
        serde_json::from_str(list).map_err(|error| format!("the Simplify list did not parse: {error}"))?;
    let mut seen_ids: HashSet<String> = known
        .iter()
        .filter_map(|posting| simplify_id(&posting.link))
        .map(str::to_owned)
        .collect();
    let mut seen_addresses: HashSet<String> = known
        .iter()
        .flat_map(|posting| [Some(posting_address(&posting.link)), posting.address.clone()])
        .flatten()
        .map(|address| comparable(&address))
        .collect();
    let since = now.as_second() - WINDOW.as_secs();
    let mut found = Vec::new();
    for listing in listings {
        let wanted = listing.active
            && listing.is_visible
            && listing.date_posted >= since
            && listing
                .category
                .as_deref()
                .is_some_and(|category| CATEGORIES.contains(&category));
        if !wanted {
            continue;
        }
        let address = apply::posting_address(&listing.url).unwrap_or(listing.url);
        let new_id = seen_ids.insert(listing.id.clone());
        let new_address = seen_addresses.insert(comparable(&address));
        if !(new_id && new_address) {
            continue;
        }
        found.push(Posting {
            link: simplify_link(&listing.id),
            found_at: now,
            source: SOURCE.to_owned(),
            company: listing.company_name,
            title: listing.title,
            location: (!listing.locations.is_empty()).then(|| listing.locations.join("; ")),
            email_subject: None,
            stage: Stage::Found,
            score: None,
            variant: None,
            flags: None,
            notes: None,
            address: Some(address),
            sponsorship: None,
        });
    }
    Ok(found)
}

/// Downloads the list and returns the postings in it that `known` does not hold.
pub async fn pull(known: &[Posting], now: Timestamp) -> Result<Vec<Posting>, String> {
    let (status, list) = description::get(SIMPLIFY_NEW_GRAD).await?;
    if status != 200 {
        return Err(format!("GitHub answered the Simplify list with status {status}"));
    }
    new_postings(&list, known, now)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const NOW: i64 = 1_800_000_000;

    fn listing(id: &str, title: &str, url: &str, category: &str, hours_ago: i64) -> serde_json::Value {
        json!({
            "source": "Simplify",
            "id": id,
            "company_name": "Example Co",
            "title": title,
            "url": url,
            "category": category,
            "locations": ["Austin, TX", "Remote in USA"],
            "active": true,
            "is_visible": true,
            "date_posted": NOW - hours_ago * 3600,
            "sponsorship": "Other",
        })
    }

    fn known(link: &str, address: Option<&str>) -> Posting {
        Posting {
            link: link.into(),
            found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
            source: "SWE List".into(),
            company: "Example Co".into(),
            title: "Engineer".into(),
            location: None,
            email_subject: None,
            stage: Stage::Review,
            score: None,
            variant: None,
            flags: None,
            notes: None,
            address: address.map(Into::into),
            sponsorship: None,
        }
    }

    #[test]
    fn new_software_listings_in_the_window_become_found_postings() {
        let ashby = "https://jobs.ashbyhq.com/example/0b5a5bb6-8a2f-4b4e-9a63-2f4c7c1d2e3f";
        let mut closed = listing(
            "closed",
            "Backend Engineer",
            "https://jobs.example.com/9",
            "Software",
            1,
        );
        closed["active"] = json!(false);
        let list = json!([
            listing(
                "a",
                "Backend Engineer",
                &format!("{ashby}/application?embed=true"),
                "Software",
                2
            ),
            listing(
                "b",
                "ML Engineer",
                "https://careers.example.com/2?utm_source=x",
                "AI/ML/Data",
                10
            ),
            listing("c", "Chip Designer", "https://careers.example.com/3", "Hardware", 1),
            listing("d", "Old Engineer", "https://careers.example.com/4", "Software", 100),
            listing("e", "Seen by email", "https://careers.example.com/5", "Software", 1),
            listing(
                "f",
                "Came through Notify",
                "https://careers.example.com/6",
                "Software",
                1
            ),
            listing("g", "Same job twice", &format!("{ashby}?utm_source=y"), "Software", 1),
            closed,
        ]);
        let known = [
            known("https://simplify.jobs/p/e?utm_source=swelist", None),
            known(
                "https://links.notify.careers/CL0/https:%2F%2Fcareers.example.com%2F6/1/x",
                None,
            ),
        ];
        let now = Timestamp::from_second(NOW).unwrap();
        let found = new_postings(&list.to_string(), &known, now).unwrap();

        let got: Vec<(&str, &str, Option<&str>)> = found
            .iter()
            .map(|p| (p.link.as_str(), p.title.as_str(), p.address.as_deref()))
            .collect();
        assert_eq!(
            got,
            [
                ("https://simplify.jobs/p/a", "Backend Engineer", Some(ashby)),
                (
                    "https://simplify.jobs/p/b",
                    "ML Engineer",
                    Some("https://careers.example.com/2?utm_source=x")
                ),
            ]
        );
        let first = &found[0];
        assert_eq!(first.stage, Stage::Found);
        assert_eq!(first.source, SOURCE);
        assert_eq!(first.found_at, now);
        assert_eq!(first.location.as_deref(), Some("Austin, TX; Remote in USA"));
    }

    #[test]
    fn a_list_that_is_not_the_expected_json_is_an_error() {
        let now = Timestamp::from_second(NOW).unwrap();
        assert!(new_postings("{\"listings\": []}", &[], now).is_err());
    }
}
