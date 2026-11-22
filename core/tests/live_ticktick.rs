//! Writes to the real TickTick account in the Keychain, so it runs only when asked:
//! `MAVERICK_E2E_TICKTICK=1 cargo test -p maverick-core --test live_ticktick -- --ignored`.

use jiff::{ToSpan as _, ZonedDateTime};
use maverick_core::ticktick::{self, Change, Due, NewTask, Priority};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "writes to the real TickTick account"]
async fn a_task_is_created_changed_finished_and_deleted() {
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
        .checked_add(30.days())
        .unwrap();
    let due = due.checked_add(10.hours()).unwrap();
    let task = NewTask {
        title: "Maverick test task, safe to delete".into(),
        list_id: "inbox".into(),
        due: Some(Due::At(due.clone())),
        remind_before: Some(30),
        priority: Priority::High,
    };
    let made = ticktick::create(&token, &task, due.time_zone()).await.unwrap();
    let outcome = async {
        assert_eq!(made.title, task.title);
        assert!(made.project_id.starts_with("inbox"), "went to {}", made.project_id);
        assert_eq!(made.priority, Priority::High.value());
        let stored = jiff::fmt::strtime::parse("%Y-%m-%dT%H:%M:%S%.f%z", made.due_date.as_deref().unwrap())
            .unwrap()
            .to_timestamp()
            .unwrap();
        assert_eq!(stored, due.timestamp());

        let lower = Change {
            priority: Some(Priority::Low),
            ..Change::default()
        };
        ticktick::change(&token, &made.id, &made.project_id, &lower, due.time_zone())
            .await
            .unwrap();
        let (_, open) = ticktick::open_tasks(&token).await.unwrap();
        let changed = open.iter().find(|open| open.id == made.id).expect("the task is open");
        assert_eq!(Priority::from_value(changed.priority), Priority::Low);

        ticktick::complete(&token, &made.project_id, &made.id).await.unwrap();
        let (_, open) = ticktick::open_tasks(&token).await.unwrap();
        assert!(
            !open.iter().any(|open| open.id == made.id),
            "a finished task is not open"
        );
    };
    outcome.await;
    ticktick::delete(&token, &made.project_id, &made.id).await.unwrap();
}
