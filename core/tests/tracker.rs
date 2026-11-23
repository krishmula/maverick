use std::io::{Cursor, Write};

use jiff::Timestamp;
use jiff::civil::date;
use maverick_core::jobs::{
    Application, ApplicationStatus, Decision, HiringPost, Outreach, Posting, Sponsorship, Stage,
};
use maverick_core::store::{Store, Summary};
use maverick_core::tracker::{self, RowProblem, Tracker, TrackerError};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

/// The ways Google Sheets writes a cell into an `.xlsx` export.
#[derive(Clone, Copy)]
enum Cell {
    Blank,
    Text(&'static str),
    Number(&'static str),
    /// Text with a link stored in the sheet's relationships.
    Linked(&'static str, &'static str),
    /// A formula and the value it last produced.
    Formula(&'static str, &'static str),
}
use Cell::{Blank, Formula, Linked, Number, Text};

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Builds a real `.xlsx` archive, so the tests read the same parts a Google Sheets export has.
fn workbook(sheets: &[(&str, Vec<Vec<Cell>>)]) -> Cursor<Vec<u8>> {
    const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const DOCUMENT_RELS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
    const PACKAGE_RELS: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

    let mut archive = ZipWriter::new(Cursor::new(Vec::new()));
    let mut add = |name: &str, content: String| {
        archive.start_file(name, SimpleFileOptions::default()).unwrap();
        archive.write_all(content.as_bytes()).unwrap();
    };
    let mut shared: Vec<&str> = Vec::new();
    let mut share = |text: &'static str| {
        shared.push(text);
        shared.len() - 1
    };

    let mut sheet_list = String::new();
    let mut sheet_rels = String::new();
    for (index, (name, rows)) in sheets.iter().enumerate() {
        let number = index + 1;
        let mut data = String::new();
        let mut links = String::new();
        let mut link_rels = String::new();
        for (row_index, row) in rows.iter().enumerate() {
            let row_number = row_index + 1;
            data += &format!(r#"<row r="{row_number}">"#);
            for (column, cell) in row.iter().enumerate() {
                let address = format!("{}{row_number}", char::from(b'A' + u8::try_from(column).unwrap()));
                match *cell {
                    Blank => {}
                    Text(text) => data += &format!(r#"<c r="{address}" t="s"><v>{}</v></c>"#, share(text)),
                    Number(number) => data += &format!(r#"<c r="{address}"><v>{number}</v></c>"#),
                    Linked(text, target) => {
                        data += &format!(r#"<c r="{address}" t="s"><v>{}</v></c>"#, share(text));
                        let id = format!("rId{}", links.matches("<hyperlink").count() + 1);
                        links += &format!(r#"<hyperlink r:id="{id}" ref="{address}"/>"#);
                        link_rels += &format!(
                            r#"<Relationship Id="{id}" Type="{DOCUMENT_RELS}/hyperlink" Target="{}" TargetMode="External"/>"#,
                            escape(target)
                        );
                    }
                    Formula(formula, value) => {
                        data += &format!(
                            r#"<c r="{address}" t="str"><f>{}</f><v>{}</v></c>"#,
                            escape(formula),
                            escape(value)
                        );
                    }
                }
            }
            data += "</row>";
        }
        let hyperlinks = if links.is_empty() {
            String::new()
        } else {
            format!("<hyperlinks>{links}</hyperlinks>")
        };
        add(
            &format!("xl/worksheets/sheet{number}.xml"),
            format!(
                r#"<worksheet xmlns="{MAIN}" xmlns:r="{DOCUMENT_RELS}"><sheetData>{data}</sheetData>{hyperlinks}</worksheet>"#
            ),
        );
        if !link_rels.is_empty() {
            add(
                &format!("xl/worksheets/_rels/sheet{number}.xml.rels"),
                format!(r#"<Relationships xmlns="{PACKAGE_RELS}">{link_rels}</Relationships>"#),
            );
        }
        sheet_list += &format!(
            r#"<sheet name="{}" sheetId="{number}" r:id="rId{number}"/>"#,
            escape(name)
        );
        sheet_rels += &format!(
            r#"<Relationship Id="rId{number}" Type="{DOCUMENT_RELS}/worksheet" Target="worksheets/sheet{number}.xml"/>"#
        );
    }
    let strings: String = shared
        .iter()
        .map(|text| format!("<si><t>{}</t></si>", escape(text)))
        .collect();
    add(
        "xl/sharedStrings.xml",
        format!(r#"<sst xmlns="{MAIN}">{strings}</sst>"#),
    );
    add(
        "xl/workbook.xml",
        format!(r#"<workbook xmlns="{MAIN}" xmlns:r="{DOCUMENT_RELS}"><sheets>{sheet_list}</sheets></workbook>"#),
    );
    add(
        "xl/_rels/workbook.xml.rels",
        format!(r#"<Relationships xmlns="{PACKAGE_RELS}">{sheet_rels}</Relationships>"#),
    );
    drop(add);
    let mut file = archive.finish().unwrap();
    file.set_position(0);
    file
}

fn application_rows() -> Vec<Vec<Cell>> {
    vec![
        vec![Text("Company"), Text("Applied On"), Text("Status"), Text("Notes")],
        vec![
            Text("Example Co Software Engineer"),
            Text("09/01/2026"),
            Text("Pending"),
        ],
        vec![
            Linked("Acme Backend Engineer", "https://jobs.example.com/acme/1?a=1&b=2"),
            Number("46267.0"),
            Text("Rejected"),
            Text("Applied through the company site."),
        ],
        vec![
            Formula(
                r#"HYPERLINK("https://jobs.example.com/zed/2","Zed — Infra Engineer")"#,
                "Zed — Infra Engineer",
            ),
            Number("46342.0"),
            Text("Pending"),
        ],
        // Google Sheets leaves a link chip out of the export, so the row has no company cell at all.
        vec![Blank, Number("46347.0"), Text("Pending")],
    ]
}

const POSTING_HEADER: [Cell; 12] = [
    Text("Found at"),
    Text("Source"),
    Text("Company"),
    Text("Title"),
    Text("Location"),
    Text("Link"),
    Text("Email subject"),
    Text("Status"),
    Text("Score"),
    Text("Variant"),
    Text("Flags"),
    Text("Notes"),
];

fn posting_rows() -> Vec<Vec<Cell>> {
    vec![
        POSTING_HEADER.to_vec(),
        vec![
            Number("46341.6805787037"),
            Text("Notify.careers"),
            Text("Example Labs"),
            Text("Software Engineer, New Grad"),
            Text("San Francisco, California, United States"),
            Linked("https://links.example.net/a/1", "https://links.example.net/a/1"),
            Text("Found 1 new job posting!"),
            Text("Review"),
            Number("82.0"),
            Text("Backend/Infra"),
            Text("Advisory: relocation needed"),
            Text("Apply-ready (>=75, no blocking flags)"),
        ],
        vec![
            Number("46200.5"),
            Text("SWE List"),
            Text("Sample Systems"),
            Text("Backend Engineer"),
            Blank,
            Text("https://links.example.net/b/2"),
            Text("New roles this week"),
            Text("Found"),
        ],
    ]
}

fn hiring_post_rows() -> Vec<Vec<Cell>> {
    vec![
        vec![
            Text("Found at"),
            Text("Post link"),
            Text("Poster"),
            Text("Company"),
            Text("Role"),
            Text("Score"),
            Text("Variant"),
            Text("Outreach Status"),
            Text("Notes"),
        ],
        vec![
            Text("2026-11-17 00:52 PT"),
            Text("https://social.example.com/posts/1"),
            Text("Pat Poster"),
            Text("Example Corp"),
            Text("Platform Engineer"),
            Number("84.0"),
            Text("Resume"),
            Text("Replied — referral in progress"),
            Text("Asked for a resume."),
        ],
        vec![
            Text("2026-07-04 09:30 PT"),
            Text("https://social.example.com/posts/2"),
            Text("Sam Sender"),
            Text("Sample Systems"),
            Text("ML Infrastructure Engineer"),
            Number("61.0"),
            Text("Resume"),
            Text("Not started"),
        ],
    ]
}

fn tracker_file() -> Cursor<Vec<u8>> {
    workbook(&[
        ("Sheet1", application_rows()),
        ("Found", posting_rows()),
        ("Hiring Posts", hiring_post_rows()),
    ])
}

fn at(time: &str) -> Timestamp {
    time.parse().unwrap()
}

#[test]
fn the_tracker_is_read_from_all_three_tabs() {
    let tracker = tracker::read(tracker_file()).unwrap();

    assert_eq!(
        tracker.applications,
        [
            Application {
                sheet_row: Some(2),
                role: Some("Example Co Software Engineer".into()),
                link: None,
                applied_on: date(2026, 9, 1),
                status: ApplicationStatus::Pending,
                notes: None,
            },
            Application {
                sheet_row: Some(3),
                role: Some("Acme Backend Engineer".into()),
                link: Some("https://jobs.example.com/acme/1?a=1&b=2".into()),
                applied_on: date(2026, 9, 2),
                status: ApplicationStatus::Rejected,
                notes: Some("Applied through the company site.".into()),
            },
            Application {
                sheet_row: Some(4),
                role: Some("Zed — Infra Engineer".into()),
                link: Some("https://jobs.example.com/zed/2".into()),
                applied_on: date(2026, 11, 16),
                status: ApplicationStatus::Pending,
                notes: None,
            },
            Application {
                sheet_row: Some(5),
                role: None,
                link: None,
                applied_on: date(2026, 11, 21),
                status: ApplicationStatus::Pending,
                notes: None,
            },
        ]
    );
    assert_eq!(
        tracker.postings,
        [
            Posting {
                link: "https://links.example.net/a/1".into(),
                found_at: at("2026-11-16T00:20:02Z"),
                source: "Notify.careers".into(),
                company: "Example Labs".into(),
                title: "Software Engineer, New Grad".into(),
                location: Some("San Francisco, California, United States".into()),
                email_subject: Some("Found 1 new job posting!".into()),
                stage: Stage::Review,
                score: Some(82),
                variant: Some("Backend/Infra".into()),
                flags: Some("Advisory: relocation needed".into()),
                notes: Some("Apply-ready (>=75, no blocking flags)".into()),
                address: None,
                sponsorship: None,
            },
            Posting {
                link: "https://links.example.net/b/2".into(),
                found_at: at("2026-06-27T19:00:00Z"),
                source: "SWE List".into(),
                company: "Sample Systems".into(),
                title: "Backend Engineer".into(),
                location: None,
                email_subject: Some("New roles this week".into()),
                stage: Stage::Found,
                score: None,
                variant: None,
                flags: None,
                notes: None,
                address: None,
                sponsorship: None,
            },
        ]
    );
    assert_eq!(
        tracker.hiring_posts,
        [
            HiringPost {
                post_link: "https://social.example.com/posts/1".into(),
                role: "Platform Engineer".into(),
                found_at: at("2026-11-17T08:52:00Z"),
                poster: "Pat Poster".into(),
                company: "Example Corp".into(),
                score: 84,
                variant: "Resume".into(),
                outreach: Outreach::Replied,
                notes: Some("[referral in progress] Asked for a resume.".into()),
            },
            HiringPost {
                post_link: "https://social.example.com/posts/2".into(),
                role: "ML Infrastructure Engineer".into(),
                found_at: at("2026-07-04T16:30:00Z"),
                poster: "Sam Sender".into(),
                company: "Sample Systems".into(),
                score: 61,
                variant: "Resume".into(),
                outreach: Outreach::NotStarted,
                notes: None,
            },
        ]
    );
}

#[test]
fn a_row_that_cannot_be_read_is_reported_and_the_rest_are_kept() {
    let mut applications = application_rows();
    applications[2][2] = Text("Ghosted");
    let mut postings = posting_rows();
    postings[1][8] = Number("82.5");
    let mut hiring_posts = hiring_post_rows();
    hiring_posts[2][1] = Text("2026-07-04 09:30 PT");
    let file = workbook(&[
        ("Sheet1", applications),
        ("Found", postings),
        ("Hiring Posts", hiring_posts),
    ]);

    let tracker = tracker::read(file).unwrap();

    assert_eq!(
        tracker.unreadable,
        [
            RowProblem {
                sheet: "Sheet1",
                row: 3,
                problem: r#""Ghosted" is not a status this import knows"#.into()
            },
            RowProblem {
                sheet: "Found",
                row: 2,
                problem: r#"the score "82.5" is not a whole number from 0 to 100"#.into()
            },
            RowProblem {
                sheet: "Hiring Posts",
                row: 3,
                problem: r#""2026-07-04 09:30 PT" is not a web address"#.into()
            },
        ]
    );
    let kept_rows: Vec<u32> = tracker.applications.iter().filter_map(|sent| sent.sheet_row).collect();
    assert_eq!(kept_rows, [2, 4, 5]);
    let kept_links: Vec<&str> = tracker.postings.iter().map(|found| found.link.as_str()).collect();
    assert_eq!(kept_links, ["https://links.example.net/b/2"]);
    let kept_posts: Vec<&str> = tracker
        .hiring_posts
        .iter()
        .map(|post| post.post_link.as_str())
        .collect();
    assert_eq!(kept_posts, ["https://social.example.com/posts/1"]);
}

#[test]
fn a_missing_tab_or_column_stops_the_import() {
    let no_posts = workbook(&[("Sheet1", application_rows()), ("Found", posting_rows())]);
    assert!(matches!(
        tracker::read(no_posts),
        Err(TrackerError::MissingSheet("Hiring Posts"))
    ));

    let mut postings = posting_rows();
    postings[0][7] = Text("State");
    let renamed = workbook(&[
        ("Sheet1", application_rows()),
        ("Found", postings),
        ("Hiring Posts", hiring_post_rows()),
    ]);
    assert!(matches!(
        tracker::read(renamed),
        Err(TrackerError::MissingColumn {
            sheet: "Found",
            column: "Status"
        })
    ));
}

fn summary_of(tracker: &Tracker, store: &mut Store) -> Summary {
    store.save_tracker(tracker).unwrap();
    store.summary().unwrap()
}

#[test]
fn a_repeated_import_changes_nothing_and_an_edited_row_updates_in_place() {
    let scratch = tempfile::tempdir().unwrap();
    let mut store = Store::open(&scratch.path().join("maverick.db")).unwrap();
    let mut tracker = tracker::read(tracker_file()).unwrap();

    let first = summary_of(&tracker, &mut store);
    assert_eq!(
        first,
        Summary {
            postings: vec![
                (Stage::Found, 1),
                (Stage::Duplicate, 0),
                (Stage::Skipped, 0),
                (Stage::Review, 1),
                (Stage::Applied, 0),
            ],
            applications: vec![(ApplicationStatus::Pending, 3), (ApplicationStatus::Rejected, 1)],
            applications_without_role: 1,
            hiring_posts: vec![
                (Outreach::NotStarted, 1),
                (Outreach::Skipped, 0),
                (Outreach::Drafted, 0),
                (Outreach::Typed, 0),
                (Outreach::Sent, 0),
                (Outreach::Replied, 1),
            ],
        }
    );
    assert_eq!(summary_of(&tracker, &mut store), first);

    tracker.postings[1].stage = Stage::Skipped;
    tracker.applications[0].status = ApplicationStatus::Rejected;
    tracker.hiring_posts[1].outreach = Outreach::Drafted;
    let edited = summary_of(&tracker, &mut store);
    assert_eq!(
        edited.postings,
        [
            (Stage::Found, 0),
            (Stage::Duplicate, 0),
            (Stage::Skipped, 1),
            (Stage::Review, 1),
            (Stage::Applied, 0)
        ]
    );
    assert_eq!(
        edited.applications,
        [(ApplicationStatus::Pending, 2), (ApplicationStatus::Rejected, 2)]
    );
    assert_eq!(
        edited.hiring_posts,
        [
            (Outreach::NotStarted, 0),
            (Outreach::Skipped, 0),
            (Outreach::Drafted, 1),
            (Outreach::Typed, 0),
            (Outreach::Sent, 0),
            (Outreach::Replied, 1)
        ]
    );

    drop(store);
    let reopened = Store::open(&scratch.path().join("maverick.db")).unwrap();
    assert_eq!(reopened.summary().unwrap(), edited);
}

#[test]
fn a_submitted_application_marks_its_posting_applied_and_is_recorded_once() {
    let scratch = tempfile::tempdir().unwrap();
    let mut store = Store::open(&scratch.path().join("maverick.db")).unwrap();
    let posting = |link: &str| Posting {
        link: link.into(),
        found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
        source: "Notify".into(),
        company: "Example Co".into(),
        title: "Backend Engineer".into(),
        location: None,
        email_subject: None,
        stage: Stage::Review,
        score: Some(80),
        variant: None,
        flags: None,
        notes: None,
        address: None,
        sponsorship: None,
    };
    let tracker = Tracker {
        postings: vec![
            posting("https://jobs.example.com/1"),
            posting("https://jobs.example.com/2"),
        ],
        ..Tracker::default()
    };
    store.save_tracker(&tracker).unwrap();

    let sent = Application::submitted_today("Backend Engineer at Example Co", "https://jobs.example.com/1");
    for _ in 0..2 {
        store
            .record_application(&["https://jobs.example.com/1".to_owned()], &sent)
            .unwrap();
    }

    let stages: Vec<Stage> = store.postings().unwrap().iter().map(|posting| posting.stage).collect();
    assert_eq!(stages, [Stage::Applied, Stage::Review]);
    assert_eq!(store.applications().unwrap(), [sent]);
}

#[test]
fn scoring_decisions_survive_a_reopen_and_a_repeated_import_keeps_the_address() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("maverick.db");
    let mut store = Store::open(&path).unwrap();
    let found = Posting {
        link: "https://board.example.com/p/1".into(),
        found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
        source: "Board".into(),
        company: "Example Co".into(),
        title: "Backend Engineer".into(),
        location: None,
        email_subject: None,
        stage: Stage::Found,
        score: None,
        variant: None,
        flags: None,
        notes: None,
        address: None,
        sponsorship: None,
    };
    let tracker = Tracker {
        postings: vec![found.clone()],
        ..Tracker::default()
    };
    store.save_tracker(&tracker).unwrap();
    let scored = Posting {
        stage: Stage::Review,
        score: Some(78),
        variant: Some("Backend/Infra".into()),
        flags: Some("Blocking: no sponsorship".into()),
        notes: Some("Review - 78".into()),
        address: Some("https://jobs.example.com/1".into()),
        sponsorship: Some(Sponsorship::NoSponsorship),
        ..found
    };
    store.save_decisions(std::slice::from_ref(&scored)).unwrap();
    drop(store);

    let mut reopened = Store::open(&path).unwrap();
    assert_eq!(reopened.postings().unwrap(), std::slice::from_ref(&scored));

    reopened.save_tracker(&tracker).unwrap();
    let after_import = &reopened.postings().unwrap()[0];
    assert_eq!(after_import.address, scored.address);
    assert_eq!(after_import.sponsorship, scored.sponsorship);
}

#[test]
fn a_decision_survives_a_sheet_sync_that_rewrites_its_posting_and_can_be_taken_back() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("maverick.db");
    let mut store = Store::open(&path).unwrap();
    let posting = |link: &str, title: &str| Posting {
        link: link.into(),
        found_at: "2026-11-01T00:00:00Z".parse().unwrap(),
        source: "Board".into(),
        company: "Example Co".into(),
        title: title.into(),
        location: None,
        email_subject: None,
        stage: Stage::Review,
        score: Some(70),
        variant: None,
        flags: None,
        notes: None,
        address: None,
        sponsorship: None,
    };
    let (first, second) = ("https://board.example.com/p/1", "https://board.example.com/p/2");
    let tracker = Tracker {
        postings: vec![posting(first, "Backend Engineer"), posting(second, "Data Engineer")],
        ..Tracker::default()
    };
    store.save_tracker(&tracker).unwrap();
    store.decide(first, Some(Decision::Approved)).unwrap();
    store.decide(second, Some(Decision::Approved)).unwrap();
    store.decide(second, Some(Decision::Skipped)).unwrap();
    drop(store);

    let mut reopened = Store::open(&path).unwrap();
    let renamed = Tracker {
        postings: vec![posting(first, "Backend Engineer II"), posting(second, "Data Engineer")],
        ..Tracker::default()
    };
    reopened.save_tracker(&renamed).unwrap();
    let decisions = reopened.decisions().unwrap();
    assert_eq!(decisions.len(), 2);
    assert_eq!(decisions[first], Decision::Approved);
    assert_eq!(decisions[second], Decision::Skipped);
    assert_eq!(reopened.postings().unwrap()[0].stage, Stage::Review);

    reopened.decide(second, None).unwrap();
    assert_eq!(reopened.decisions().unwrap().keys().collect::<Vec<_>>(), [first]);
}

#[test]
fn intake_adds_only_postings_the_store_does_not_hold() {
    let scratch = tempfile::tempdir().unwrap();
    let mut store = Store::open(&scratch.path().join("maverick.db")).unwrap();
    let found = |link: &str| Posting {
        link: link.into(),
        found_at: "2026-11-22T17:00:00Z".parse().unwrap(),
        source: "Simplify".into(),
        company: "Example Co".into(),
        title: "Backend Engineer".into(),
        location: Some("Austin, TX".into()),
        email_subject: None,
        stage: Stage::Found,
        score: None,
        variant: None,
        flags: None,
        notes: None,
        address: Some(format!("{link}/original")),
        sponsorship: None,
    };
    assert_eq!(store.add_found(&[found("https://simplify.jobs/p/1")]).unwrap(), 1);
    let scored = Posting {
        stage: Stage::Review,
        score: Some(70),
        ..found("https://simplify.jobs/p/1")
    };
    store.save_decisions(std::slice::from_ref(&scored)).unwrap();

    let again = [found("https://simplify.jobs/p/1"), found("https://simplify.jobs/p/2")];
    assert_eq!(store.add_found(&again).unwrap(), 1);
    let postings = store.postings().unwrap();
    assert_eq!(postings, [scored, found("https://simplify.jobs/p/2")]);
}
