//! Asks the real model with the real TickTick account, confirms the task it proposes,
//! and deletes it again. It runs only when asked:
//! `MAVERICK_E2E_MAV=1 cargo test -p maverick-core --test live_mav -- --ignored`.

use jiff::{ToSpan as _, ZonedDateTime};
use maverick_core::mav::{self, Action, Context};
use maverick_core::ticktick::{self, Due};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "calls the model and writes to the real TickTick account"]
async fn a_request_becomes_a_task_that_lands_in_ticktick() {
    if std::env::var("MAVERICK_E2E_MAV").is_err() {
        return;
    }
    let now = ZonedDateTime::now();
    let context = Context::read(now.clone()).await.unwrap();
    let reply = mav::ask(
        "remind me to check the Maverick test task tomorrow at 9am",
        &[],
        &context,
    )
    .await
    .unwrap();
    println!("Mav: {}", reply.text);
    let [Ok(action)] = &reply.proposals[..] else {
        panic!("expected one usable proposal, got {:?}", reply.proposals)
    };
    let Action::Task { task, .. } = action else {
        panic!("expected a task, got {action:?}")
    };
    let tomorrow_nine = now
        .date()
        .tomorrow()
        .unwrap()
        .at(9, 0, 0, 0)
        .to_zoned_datetime(now.time_zone().clone())
        .unwrap();
    assert_eq!(task.due, Some(Due::At(tomorrow_nine.clone())));

    println!("{}", action.carry_out().await.unwrap());
    let token = ticktick::stored_token().await.unwrap().unwrap();
    let start = now.start_of_day().unwrap();
    let found = ticktick::undone(
        &token,
        start.timestamp(),
        start.checked_add(3.days()).unwrap().timestamp(),
    )
    .await
    .unwrap()
    .into_iter()
    .find(|made| made.title == task.title)
    .expect("the confirmed task is in TickTick");
    ticktick::delete(&token, &found.project_id, &found.id).await.unwrap();
}
