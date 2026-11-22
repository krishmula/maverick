//! Writes to the real Google calendar of the first profile that allows it, so it runs only when asked:
//! `MAVERICK_E2E_CALENDAR=1 cargo test -p maverick-core --test live_calendar -- --ignored`.

use jiff::{ToSpan as _, ZonedDateTime};
use maverick_core::calendar::{self, Span};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes to the real Google calendar"]
async fn a_block_is_added_moved_and_deleted() {
    if std::env::var("MAVERICK_E2E_CALENDAR").is_err() {
        return;
    }
    let title = "Maverick test block, safe to delete";
    let day = ZonedDateTime::now()
        .start_of_day()
        .unwrap()
        .checked_add(30.days())
        .unwrap();
    let start = day.checked_add(3.hours()).unwrap();
    let end = start.checked_add(30.minutes()).unwrap();
    let find = || async {
        calendar::events(
            day.timestamp(),
            day.checked_add(1.days()).unwrap().timestamp(),
            day.time_zone(),
        )
        .await
        .unwrap()
        .into_iter()
        .find(|event| event.title == title)
    };
    let profile = calendar::add(title, &start, &end, None).await.unwrap();
    println!("added to {profile}");
    let added = find().await.expect("the block is on the calendar");
    assert_eq!(added.span, Span::At(start.clone(), end.clone()));

    let later = start.checked_add(1.hours()).unwrap();
    let later_end = end.checked_add(1.hours()).unwrap();
    calendar::reschedule(&added, &later, &later_end).await.unwrap();
    let moved = find().await.expect("the block is still there");
    assert_eq!(moved.span, Span::At(later, later_end));

    calendar::delete(&moved).await.unwrap();
    assert!(find().await.is_none(), "the block is gone");
}
