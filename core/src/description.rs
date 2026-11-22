//! Reads a posting's job description over plain HTTP, with no browser.
//!
//! Each source has a public address that returns the posting as JSON or as a page with the
//! posting embedded. Requests go through `/usr/bin/curl`, so Maverick needs no HTTP library.

use std::fmt::Write as _;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use crate::text::{decoded, plain};
use crate::{apply, ashby, greenhouse, workday};

const TIME_LIMIT: Duration = Duration::from_secs(30);
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko)";

/// Where a posting's description can be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// A job board that copies postings from many systems and links to the original.
    Simplify,
    Ashby,
    Greenhouse,
    Lever,
    Workday,
    Amazon,
}

impl Source {
    pub fn of(address: &str) -> Option<Self> {
        let host = host(address)?;
        Some(match host {
            "simplify.jobs" => Self::Simplify,
            "jobs.lever.co" | "jobs.eu.lever.co" => Self::Lever,
            "amazon.jobs" | "www.amazon.jobs" => Self::Amazon,
            _ if ashby::is_posting(address) => Self::Ashby,
            _ if greenhouse::is_posting(address) => Self::Greenhouse,
            _ if workday::is_posting(address) => Self::Workday,
            _ => return None,
        })
    }
}

/// What a source says about a posting.
#[derive(Clone, Debug, PartialEq)]
pub enum Fetched {
    Open(Description),
    /// The source answered and says the posting is no longer listed.
    Closed,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Description {
    pub title: String,
    pub location: Option<String>,
    /// The job description as plain text.
    pub text: String,
    /// The posting's own address, when the source is a board that links to it.
    pub address: Option<String>,
}

/// Reads the posting at `address`. An address no source covers is an error.
pub async fn fetch(address: &str) -> Result<Fetched, String> {
    let source = Source::of(address).ok_or("no source Maverick can read covers this address")?;
    match source {
        Source::Simplify => {
            let (status, page) = get(address).await?;
            let mut fetched = simplify(status, &page)?;
            if let Fetched::Open(description) = &mut fetched
                && let Some(click) = simplify_click(&page)
            {
                description.address = redirect(&click)
                    .await
                    .ok()
                    .flatten()
                    .map(|to| apply::posting_address(&to).unwrap_or(to));
            }
            Ok(fetched)
        }
        Source::Ashby => {
            let posting = ashby::posting_address(address).ok_or("not an Ashby posting")?;
            let (status, page) = get(&posting).await?;
            ashby_page(status, &page)
        }
        Source::Greenhouse => {
            let api = greenhouse_api(address).ok_or("not a Greenhouse posting")?;
            let (status, body) = get(&api).await?;
            greenhouse_reply(status, &body)
        }
        Source::Lever => {
            let api = lever_api(address).ok_or("not a Lever posting")?;
            let (status, body) = get(&api).await?;
            lever_reply(status, &body)
        }
        Source::Workday => {
            let api = workday::job_data_address(address).ok_or("not a Workday posting")?;
            let (status, body) = get(&api).await?;
            workday_reply(status, &body)
        }
        Source::Amazon => {
            let id = amazon_id(address).ok_or("not an Amazon posting")?;
            let api = format!("https://www.amazon.jobs/en/search.json?job_id_icims[]={id}&result_limit=5");
            let (status, body) = get(&api).await?;
            amazon_reply(status, &body, id)
        }
    }
}

fn host(address: &str) -> Option<&str> {
    address.strip_prefix("https://")?.split(['/', '?', '#']).next()
}

fn path_parts(address: &str) -> Vec<&str> {
    let path = address
        .strip_prefix("https://")
        .and_then(|rest| rest.split_once('/'))
        .map_or("", |(_, path)| path);
    let path = path.split(['?', '#']).next().unwrap_or("");
    path.split('/').filter(|part| !part.is_empty()).collect()
}

/// A JSON reply, or a clear error when the source answered with something else.
fn json(status: u16, body: &str) -> Result<Value, String> {
    if status != 200 {
        return Err(format!("the source answered with status {status}"));
    }
    serde_json::from_str(body).map_err(|_| "the source's reply was not JSON".to_owned())
}

fn text_of(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// Fails when a source gave a posting with no description, which is no basis for a score.
fn open(description: Description) -> Result<Fetched, String> {
    if description.text.len() < 200 {
        return Err("the posting's description is empty or too short to judge".to_owned());
    }
    Ok(Fetched::Open(description))
}

// Simplify: the page embeds the posting as Next.js data.

fn next_data(page: &str) -> Option<Value> {
    let start = page.find("<script id=\"__NEXT_DATA__\"")?;
    let body = &page[start..];
    let body = &body[body.find('>')? + 1..];
    serde_json::from_str(&body[..body.find("</script>")?]).ok()
}

fn simplify(status: u16, page: &str) -> Result<Fetched, String> {
    if status == 404 {
        return Ok(Fetched::Closed);
    }
    if status != 200 {
        return Err(format!("the source answered with status {status}"));
    }
    let data = next_data(page).ok_or("the Simplify page holds no posting data")?;
    let posting = &data["props"]["pageProps"]["jobPosting"];
    if posting.is_null() {
        return Ok(Fetched::Closed);
    }
    if posting["active"] == false || posting["archive"] == true {
        return Ok(Fetched::Closed);
    }
    let mut text = plain(posting["description"].as_str().unwrap_or(""));
    for (heading, key) in [
        ("Responsibilities", "responsibilities"),
        ("Requirements", "requirements"),
        ("Nice to have", "desirable"),
    ] {
        let items: Vec<&str> = posting[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        if !items.is_empty() {
            let _ = write!(text, "\n{heading}:");
            for item in items {
                let _ = write!(text, "\n- {item}");
            }
        }
    }
    let locations: Vec<&str> = posting["locations"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|place| place["value"].as_str())
        .collect();
    open(Description {
        title: text_of(&posting["title"]).unwrap_or_default(),
        location: (!locations.is_empty()).then(|| locations.join("; ")),
        text: text.trim().to_owned(),
        address: None,
    })
}

/// The Simplify address that redirects to the original posting.
fn simplify_click(page: &str) -> Option<String> {
    text_of(&next_data(page)?["props"]["pageProps"]["jobPosting"]["url"])
}

// Ashby: the posting page embeds `window.__appData`, and `posting` is null once it closes.

fn ashby_page(status: u16, page: &str) -> Result<Fetched, String> {
    if status == 404 {
        return Ok(Fetched::Closed);
    }
    if status != 200 {
        return Err(format!("the source answered with status {status}"));
    }
    let start = page
        .find("window.__appData")
        .ok_or("the Ashby page holds no posting data")?;
    let body = &page[start..];
    let body = body[body.find('=').ok_or("the Ashby page holds no posting data")? + 1..].trim_start();
    // The object ends where the statement does: the first `;` at the end of a line.
    let end = body
        .find(";\n")
        .or_else(|| body.find(";</script>"))
        .unwrap_or(body.len());
    let data: Value = serde_json::from_str(&body[..end]).map_err(|_| "the Ashby posting data did not parse")?;
    let posting = &data["posting"];
    if posting.is_null() || posting["isListed"] == false {
        return Ok(Fetched::Closed);
    }
    let mut places: Vec<String> = text_of(&posting["locationName"]).into_iter().collect();
    places.extend(
        posting["secondaryLocationNames"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text_of),
    );
    let mut text = plain(posting["descriptionHtml"].as_str().unwrap_or(""));
    if let Some(workplace) = text_of(&posting["workplaceType"]) {
        let _ = write!(text, "\nWorkplace: {workplace}");
    }
    open(Description {
        title: text_of(&posting["title"]).unwrap_or_default(),
        location: (!places.is_empty()).then(|| places.join("; ")),
        text,
        address: None,
    })
}

// Greenhouse: the public board API. `content` is HTML with its markup escaped once more.

fn greenhouse_api(address: &str) -> Option<String> {
    let posting = greenhouse::posting_address(address)?;
    let parts = path_parts(&posting);
    let [board, "jobs", id] = parts[..] else { return None };
    Some(format!("https://boards-api.greenhouse.io/v1/boards/{board}/jobs/{id}"))
}

fn greenhouse_reply(status: u16, body: &str) -> Result<Fetched, String> {
    if status == 404 {
        return Ok(Fetched::Closed);
    }
    let job = json(status, body)?;
    open(Description {
        title: text_of(&job["title"]).unwrap_or_default(),
        location: text_of(&job["location"]["name"]),
        text: plain(&decoded(job["content"].as_str().unwrap_or(""))),
        address: None,
    })
}

// Lever: the public postings API.

fn lever_api(address: &str) -> Option<String> {
    let api = match host(address)? {
        "jobs.lever.co" => "api.lever.co",
        "jobs.eu.lever.co" => "api.eu.lever.co",
        _ => return None,
    };
    let parts = path_parts(address);
    let (company, id) = (parts.first()?, parts.get(1)?);
    let is_id = id.len() == 36 && id.bytes().all(|byte| byte.is_ascii_hexdigit() || byte == b'-');
    is_id.then(|| format!("https://{api}/v0/postings/{company}/{id}"))
}

fn lever_reply(status: u16, body: &str) -> Result<Fetched, String> {
    if status == 404 {
        return Ok(Fetched::Closed);
    }
    let posting = json(status, body)?;
    let mut text = posting["descriptionPlain"].as_str().unwrap_or("").trim().to_owned();
    for list in posting["lists"].as_array().into_iter().flatten() {
        let _ = write!(
            text,
            "\n{}:\n{}",
            list["text"].as_str().unwrap_or(""),
            plain(list["content"].as_str().unwrap_or(""))
        );
    }
    if let Some(more) = text_of(&posting["additionalPlain"]) {
        let _ = write!(text, "\n{more}");
    }
    open(Description {
        title: text_of(&posting["text"]).unwrap_or_default(),
        location: text_of(&posting["categories"]["location"]),
        text,
        address: None,
    })
}

// Workday: the job data each career site serves to its own pages.

fn workday_reply(status: u16, body: &str) -> Result<Fetched, String> {
    if status == 404 {
        return Ok(Fetched::Closed);
    }
    let reply = json(status, body)?;
    let info = &reply["jobPostingInfo"];
    if info.is_null() || info["canApply"] == false {
        return Ok(Fetched::Closed);
    }
    let mut places: Vec<String> = text_of(&info["location"]).into_iter().collect();
    places.extend(
        info["additionalLocations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(text_of),
    );
    open(Description {
        title: text_of(&info["title"]).unwrap_or_default(),
        location: (!places.is_empty()).then(|| places.join("; ")),
        text: plain(info["jobDescription"].as_str().unwrap_or("")),
        address: None,
    })
}

// Amazon: the search API finds one posting by its number.

fn amazon_id(address: &str) -> Option<&str> {
    path_parts(address)
        .into_iter()
        .skip_while(|part| *part != "jobs")
        .nth(1)
        .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
}

fn amazon_reply(status: u16, body: &str, id: &str) -> Result<Fetched, String> {
    let reply = json(status, body)?;
    let found = reply["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|job| job["id_icims"].as_str() == Some(id));
    let Some(job) = found else { return Ok(Fetched::Closed) };
    let mut text = plain(job["description"].as_str().unwrap_or(""));
    for (heading, key) in [
        ("Basic qualifications", "basic_qualifications"),
        ("Preferred qualifications", "preferred_qualifications"),
    ] {
        if let Some(html) = text_of(&job[key]) {
            let _ = write!(text, "\n{heading}:\n{}", plain(&html));
        }
    }
    open(Description {
        title: text_of(&job["title"]).unwrap_or_default(),
        location: text_of(&job["normalized_location"]).or_else(|| text_of(&job["location"])),
        text,
        address: None,
    })
}

// HTTP through curl.

/// The status and body of `address`, following redirects.
pub(crate) async fn get(address: &str) -> Result<(u16, String), String> {
    let run = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--location", "--compressed"])
        .args(["--max-time", &TIME_LIMIT.as_secs().to_string()])
        .args(["--user-agent", USER_AGENT, "--write-out", "\n%{http_code}"])
        .arg("--")
        .arg(address)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = run.await.map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "the request failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text.rsplit_once('\n').ok_or("curl gave no status")?;
    let status = status.trim().parse().map_err(|_| "curl gave no status")?;
    Ok((status, body.to_owned()))
}

/// Where `address` redirects to, without following it.
async fn redirect(address: &str) -> Result<Option<String>, String> {
    let output = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--output", "/dev/null"])
        .args(["--max-time", &TIME_LIMIT.as_secs().to_string()])
        .args(["--user-agent", USER_AGENT, "--write-out", "%{redirect_url}"])
        .arg("--")
        .arg(address)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| format!("could not run curl: {error}"))?;
    let to = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    Ok(to.starts_with("https://").then_some(to))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    const FILLER: &str = "We are looking for an engineer to build and run the services behind our product. \
        You will design APIs, own data pipelines, and keep production healthy with the rest of the team. \
        Our stack is Rust, Go, Postgres and Kubernetes on AWS.";

    fn description(fetched: Fetched) -> Description {
        match fetched {
            Fetched::Open(description) => description,
            Fetched::Closed => panic!("the posting should be open"),
        }
    }

    #[test]
    fn each_address_maps_to_its_source() {
        let cases = [
            (
                "https://simplify.jobs/p/b98f5eb8-103b-4503-af0d-7e7d64d11f97?utm_source=x",
                Some(Source::Simplify),
            ),
            (
                "https://jobs.ashbyhq.com/example/0b5a5bb6-8a2f-4b4e-9a63-2f4c7c1d2e3f",
                Some(Source::Ashby),
            ),
            (
                "https://job-boards.greenhouse.io/example/jobs/4012345",
                Some(Source::Greenhouse),
            ),
            (
                "https://jobs.lever.co/example/0b5a5bb6-8a2f-4b4e-9a63-2f4c7c1d2e3f/apply",
                Some(Source::Lever),
            ),
            (
                "https://example.wd5.myworkdayjobs.com/en-US/Careers/job/Austin-TX/Engineer_R-1",
                Some(Source::Workday),
            ),
            (
                "https://www.amazon.jobs/en/jobs/3012345/software-dev-engineer",
                Some(Source::Amazon),
            ),
            ("https://careers.example.com/jobs/42", None),
        ];
        for (address, source) in cases {
            assert_eq!(Source::of(address), source, "{address}");
        }
    }

    #[test]
    fn api_addresses_are_built_from_the_posting() {
        assert_eq!(
            greenhouse_api("https://boards.greenhouse.io/example/jobs/4012345?gh_src=x").as_deref(),
            Some("https://boards-api.greenhouse.io/v1/boards/example/jobs/4012345")
        );
        assert_eq!(
            lever_api("https://jobs.lever.co/example/0b5a5bb6-8a2f-4b4e-9a63-2f4c7c1d2e3f/apply").as_deref(),
            Some("https://api.lever.co/v0/postings/example/0b5a5bb6-8a2f-4b4e-9a63-2f4c7c1d2e3f")
        );
        assert_eq!(
            amazon_id("https://www.amazon.jobs/en/jobs/3012345/software-dev-engineer"),
            Some("3012345")
        );
        assert_eq!(
            workday::job_data_address("https://example.wd5.myworkdayjobs.com/en-US/Careers/job/Austin-TX/Engineer_R-1")
                .as_deref(),
            Some("https://example.wd5.myworkdayjobs.com/wday/cxs/example/Careers/job/Engineer_R-1")
        );
    }

    #[test]
    fn a_simplify_page_gives_the_description_lists_and_places() {
        let data = json!({ "props": { "pageProps": { "jobPosting": {
            "title": "Backend Engineer",
            "active": true,
            "archive": false,
            "description": format!("<p>{FILLER}</p>"),
            "requirements": ["2 years of backend work"],
            "responsibilities": [],
            "desirable": ["Kubernetes"],
            "locations": [{ "value": "Austin, TX" }, { "value": "Remote in USA" }],
            "url": "https://simplify.jobs/jobs/click/1",
        } } } });
        let page = format!("<html><script id=\"__NEXT_DATA__\" type=\"application/json\">{data}</script></html>");
        let got = description(simplify(200, &page).unwrap());
        assert_eq!(got.title, "Backend Engineer");
        assert_eq!(got.location.as_deref(), Some("Austin, TX; Remote in USA"));
        assert!(got.text.starts_with("We are looking"));
        assert!(
            got.text
                .ends_with("Requirements:\n- 2 years of backend work\nNice to have:\n- Kubernetes")
        );
        assert_eq!(
            simplify_click(&page).as_deref(),
            Some("https://simplify.jobs/jobs/click/1")
        );

        let closed = page.replace("\"active\":true", "\"active\":false");
        assert_eq!(simplify(200, &closed), Ok(Fetched::Closed));
    }

    #[test]
    fn an_ashby_page_gives_the_posting_and_a_null_posting_is_closed() {
        let data = json!({ "organization": {}, "posting": {
            "title": "Platform Engineer",
            "locationName": "New York",
            "secondaryLocationNames": ["Remote - US"],
            "workplaceType": "Hybrid",
            "descriptionHtml": format!("<p>{FILLER}</p>"),
            "isListed": true,
        } });
        let page = format!("<script>\n  window.__appData = {data};\n</script>");
        let got = description(ashby_page(200, &page).unwrap());
        assert_eq!(got.title, "Platform Engineer");
        assert_eq!(got.location.as_deref(), Some("New York; Remote - US"));
        assert!(got.text.ends_with("Workplace: Hybrid"));

        let closed = format!(
            "<script>\n  window.__appData = {};\n</script>",
            json!({ "posting": null })
        );
        assert_eq!(ashby_page(200, &closed), Ok(Fetched::Closed));
    }

    #[test]
    fn greenhouse_content_is_unescaped_before_its_markup_is_removed() {
        let body = json!({
            "title": "Software Engineer",
            "location": { "name": "Remote" },
            "content": format!("&lt;p&gt;{FILLER} &amp;amp; more&lt;/p&gt;"),
        });
        let got = description(greenhouse_reply(200, &body.to_string()).unwrap());
        assert!(got.text.ends_with("AWS. & more"), "{}", got.text);
        assert_eq!(greenhouse_reply(404, ""), Ok(Fetched::Closed));
    }

    #[test]
    fn lever_lists_follow_the_description() {
        let body = json!({
            "text": "Infrastructure Engineer",
            "categories": { "location": "San Francisco" },
            "descriptionPlain": FILLER,
            "lists": [{ "text": "What you bring", "content": "<li>Terraform</li><li>Go</li>" }],
            "additionalPlain": "We sponsor visas.",
        });
        let got = description(lever_reply(200, &body.to_string()).unwrap());
        assert!(
            got.text
                .ends_with("What you bring:\n- Terraform\n- Go\nWe sponsor visas."),
            "{}",
            got.text
        );
    }

    #[test]
    fn workday_and_amazon_say_closed_when_the_posting_is_gone() {
        let open_job = json!({ "jobPostingInfo": {
            "title": "Engineer", "location": "Austin", "canApply": true, "jobDescription": format!("<p>{FILLER}</p>"),
        } });
        assert_eq!(
            description(workday_reply(200, &open_job.to_string()).unwrap()).title,
            "Engineer"
        );
        let shut = json!({ "jobPostingInfo": { "title": "Engineer", "canApply": false } });
        assert_eq!(workday_reply(200, &shut.to_string()), Ok(Fetched::Closed));

        let none = json!({ "hits": 0, "jobs": [] });
        assert_eq!(amazon_reply(200, &none.to_string(), "3012345"), Ok(Fetched::Closed));
        let other = json!({ "jobs": [{ "id_icims": "999", "title": "Other", "description": FILLER }] });
        assert_eq!(amazon_reply(200, &other.to_string(), "3012345"), Ok(Fetched::Closed));
        let hit = json!({ "jobs": [{
            "id_icims": "3012345", "title": "SDE", "normalized_location": "Seattle, WA, USA",
            "description": FILLER, "basic_qualifications": "- 1+ years of coding",
        }] });
        let got = description(amazon_reply(200, &hit.to_string(), "3012345").unwrap());
        assert!(got.text.ends_with("Basic qualifications:\n- 1+ years of coding"));
    }

    #[test]
    fn a_description_too_short_to_judge_is_an_error() {
        let body = json!({ "title": "Engineer", "content": "&lt;p&gt;Apply now&lt;/p&gt;" });
        assert!(greenhouse_reply(200, &body.to_string()).is_err());
    }
}
