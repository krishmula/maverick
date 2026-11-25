//! macOS notifications for work that finishes while the user is elsewhere. macOS shows them only
//! while Maverick is not the frontmost app, and only when it runs from its app bundle.

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNMutableNotificationContent, UNNotificationRequest, UNNotificationSound,
    UNUserNotificationCenter,
};

use crate::queue::Step;

/// What one notification says.
#[derive(Clone, Debug, PartialEq)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// Notifications that share a group stack together in Notification Center.
    pub group: &'static str,
}

/// The notice for a fill that just ended, if its step is one the user acts on.
pub fn fill_ended(title: &str, step: &Step) -> Option<Notice> {
    let (heading, body) = match step {
        Step::Ready => (
            "Ready to submit",
            "Every required field is answered. Review it and click Submit.".to_owned(),
        ),
        Step::NeedsYou(why) => ("Needs you", why.clone()),
        Step::Failed(why) => ("Fill failed", format!("Nothing was submitted. {why}")),
        Step::Queued | Step::Opening | Step::Filling | Step::Applied => return None,
    };
    Some(Notice {
        title: format!("{heading}: {title}"),
        body,
        group: "fills",
    })
}

/// The notice for an hourly check that scored new apply-ready postings.
pub fn apply_ready(count: usize) -> Option<Notice> {
    let title = match count {
        0 => return None,
        1 => "1 new apply-ready posting".to_owned(),
        count => format!("{count} new apply-ready postings"),
    };
    Some(Notice {
        title,
        body: "Approve them on the Jobs tab.".into(),
        group: "jobs",
    })
}

/// The notice for a hiring-post sweep that drafted notes.
pub fn post_drafts(count: usize) -> Option<Notice> {
    let title = match count {
        0 => return None,
        1 => "1 new hiring-post draft".to_owned(),
        count => format!("{count} new hiring-post drafts"),
    };
    Some(Notice {
        title,
        body: "Read them on the Posts tab. Nothing was sent.".into(),
        group: "posts",
    })
}

/// True when the process runs from an app bundle. Outside one, the notification center
/// raises an Objective-C exception, so `cargo run` and tests post nothing.
fn bundled() -> bool {
    NSBundle::mainBundle().bundleIdentifier().is_some()
}

/// Asks macOS once for permission to show notifications. Later calls return the first answer.
pub fn ask_permission() {
    if !bundled() {
        return;
    }
    let answered = RcBlock::new(|granted: Bool, error: *mut NSError| {
        if !granted.as_bool() {
            // SAFETY: the handler gets either null or a valid NSError.
            let why = unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string());
            eprintln!(
                "maverick: notifications are off{}",
                why.map(|why| format!(": {why}")).unwrap_or_default()
            );
        }
    });
    UNUserNotificationCenter::currentNotificationCenter().requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &answered,
    );
}

/// `maverick notify [title]`: asks for permission, shows one notification, and says what macOS
/// answered. Run it as `Maverick.app/Contents/MacOS/maverick notify`, since only a bundle can notify.
pub fn try_one(title: &str) -> Result<(), Box<dyn std::error::Error>> {
    use std::sync::mpsc;
    use std::time::Duration;

    if !bundled() {
        return Err("run this from inside Maverick.app; a bare binary cannot show notifications".into());
    }
    let center = UNUserNotificationCenter::currentNotificationCenter();
    let (answer, answered) = mpsc::channel();
    let handler = RcBlock::new(move |granted: Bool, error: *mut NSError| {
        // SAFETY: the handler gets either null or a valid NSError.
        let why = unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string());
        let _ = answer.send((granted.as_bool(), why));
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
        &handler,
    );
    match answered.recv_timeout(Duration::from_secs(60)) {
        Ok((true, _)) => {}
        Ok((false, why)) => {
            return Err(format!(
                "notifications are off: {}",
                why.unwrap_or("turned off in System Settings".into())
            )
            .into());
        }
        Err(_) => return Err("macOS did not answer the permission request".into()),
    }
    let (done, finished) = mpsc::channel();
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(title));
    content.setBody(&NSString::from_str("A test from maverick notify."));
    let request =
        UNNotificationRequest::requestWithIdentifier_content_trigger(&NSString::from_str("try-one"), &content, None);
    let sent = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the handler gets either null or a valid NSError.
        let _ = done.send(unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string()));
    });
    center.addNotificationRequest_withCompletionHandler(&request, Some(&sent));
    match finished.recv_timeout(Duration::from_secs(10)) {
        Ok(None) => {
            println!("shown: {title}");
            Ok(())
        }
        Ok(Some(why)) => Err(format!("not shown: {why}").into()),
        Err(_) => Err("macOS did not confirm the notification".into()),
    }
}

/// Shows `notice` now. Safe to call from any thread.
pub fn post(notice: Notice) {
    if !bundled() {
        return;
    }
    let content = UNMutableNotificationContent::new();
    content.setTitle(&NSString::from_str(&notice.title));
    content.setBody(&NSString::from_str(&notice.body));
    content.setThreadIdentifier(&NSString::from_str(notice.group));
    content.setSound(Some(&UNNotificationSound::defaultSound()));
    let id = NSString::from_str(&format!("{}-{}", notice.group, jiff::Timestamp::now().as_nanosecond()));
    let request = UNNotificationRequest::requestWithIdentifier_content_trigger(&id, &content, None);
    let sent = RcBlock::new(|error: *mut NSError| {
        // SAFETY: the handler gets either null or a valid NSError.
        if let Some(error) = unsafe { error.as_ref() } {
            eprintln!(
                "maverick: a notification was not shown: {}",
                error.localizedDescription()
            );
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .addNotificationRequest_withCompletionHandler(&request, Some(&sent));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fill_notifies_only_when_it_ends_on_something_for_the_user() {
        let said = |step: Step| fill_ended("Engineer, Example Co", &step).map(|notice| (notice.title, notice.body));
        assert_eq!(
            said(Step::Ready),
            Some((
                "Ready to submit: Engineer, Example Co".into(),
                "Every required field is answered. Review it and click Submit.".into()
            ))
        );
        assert_eq!(
            said(Step::NeedsYou("2 required fields need your answer.".into())),
            Some((
                "Needs you: Engineer, Example Co".into(),
                "2 required fields need your answer.".into()
            ))
        );
        assert_eq!(
            said(Step::Failed("the page did not load".into())),
            Some((
                "Fill failed: Engineer, Example Co".into(),
                "Nothing was submitted. the page did not load".into()
            ))
        );
        for quiet in [Step::Queued, Step::Opening, Step::Filling, Step::Applied] {
            assert_eq!(said(quiet), None);
        }
    }

    #[test]
    fn checks_and_sweeps_notify_only_when_they_found_something() {
        assert_eq!(apply_ready(0), None);
        assert_eq!(apply_ready(1).unwrap().title, "1 new apply-ready posting");
        assert_eq!(apply_ready(3).unwrap().title, "3 new apply-ready postings");
        assert_eq!(post_drafts(0), None);
        assert_eq!(post_drafts(2).unwrap().title, "2 new hiring-post drafts");
    }

    #[test]
    fn posting_outside_an_app_bundle_does_nothing() {
        assert!(!bundled());
        post(apply_ready(1).unwrap());
    }
}
