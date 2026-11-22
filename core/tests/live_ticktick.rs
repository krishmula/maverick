//! Writes to the real TickTick account in the Keychain, so it runs only when asked:
//! `MAVERICK_E2E_TICKTICK=1 cargo test -p maverick-core --test live_ticktick -- --ignored`.

use jiff::ZonedDateTime;
use maverick_core::ticktick::{self, Due, NewTask};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes to the real TickTick account"]
async fn a_task_is_created_with_its_due_time_and_then_deleted() {
    if std::env::var("MAVERICK_E2E_TICKTICK").is_err() {
        return;
    }
    let token = ticktick::stored_token()
        .await
        .unwrap()
        .expect("a TickTick token in the Keychain");
    let due = ZonedDateTime::now()
        .start_of_day()
        .unwrap()
        .checked_add(jiff::ToSpan::days(30))
        .unwrap();
    let due = due.checked_add(jiff::ToSpan::hours(10)).unwrap();
    let task = NewTask {
        title: "Maverick test task, safe to delete".into(),
        list_id: "inbox".into(),
        due: Some(Due::At(due.clone())),
        remind_before: Some(30),
    };
    let made = ticktick::create(&token, &task, due.time_zone()).await.unwrap();
    let deleted = ticktick::delete(&token, &made.project_id, &made.id).await;
    assert_eq!(made.title, task.title);
    assert!(made.project_id.starts_with("inbox"), "went to {}", made.project_id);
    let stored = jiff::fmt::strtime::parse("%Y-%m-%dT%H:%M:%S%.f%z", made.due_date.as_deref().unwrap())
        .unwrap()
        .to_timestamp()
        .unwrap();
    assert_eq!(stored, due.timestamp());
    deleted.unwrap();
}
