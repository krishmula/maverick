//! Sign-in codes that a site emails to the user, read from their connected Gmail.

use std::time::Duration;

use jiff::Timestamp;
use serde_json::Value;

use crate::google::{self, Client, GMAIL_READ};

const MESSAGES: &str = "https://gmail.googleapis.com/gmail/v1/users/me/messages";
/// How long a site may take to send the code.
const CODE_WAIT: Duration = Duration::from_secs(90);

/// The newest code in a mail to `email` that matches the Gmail search `query` and arrived after
/// `since`. Waits for the mail to arrive. `None` when `email` is not a connected Gmail account
/// that can read mail, or no code came in time.
pub async fn sign_in_code(email: &str, query: &str, since: Timestamp) -> Result<Option<String>, String> {
    let file = google::accounts_path().ok_or("this system has no application data directory")?;
    let connected = google::accounts(&file)?
        .iter()
        .any(|account| account.email.eq_ignore_ascii_case(email) && account.can(GMAIL_READ));
    if !connected {
        return Ok(None);
    }
    let client = Client::stored().await?.ok_or("Google is not set up")?;
    let token = google::access_token(&client, email).await?;
    let search = format!(
        "{MESSAGES}?maxResults=5&q={}",
        crate::http::form_encoded(&format!("{query} newer_than:1h"))
    );
    let deadline = Timestamp::now() + CODE_WAIT;
    while Timestamp::now() < deadline {
        let found = google::call(&token, "GET", &search, None).await?;
        for id in found["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m["id"].as_str())
        {
            let message = google::call(&token, "GET", &format!("{MESSAGES}/{id}?format=full"), None).await?;
            if let Some(code) = code_after(&message, since) {
                return Ok(Some(code));
            }
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
    }
    Ok(None)
}

/// The code in a Gmail message, if it arrived after `since`.
fn code_after(message: &Value, since: Timestamp) -> Option<String> {
    let arrived: i64 = message["internalDate"].as_str()?.parse().ok()?;
    if arrived < since.as_millisecond() {
        return None;
    }
    code_in(message["snippet"].as_str().unwrap_or_default()).or_else(|| code_in(&body(&message["payload"])))
}

/// Every text part of a message payload, decoded.
fn body(part: &Value) -> String {
    use base64::Engine as _;
    let own = part["body"]["data"]
        .as_str()
        .filter(|_| part["mimeType"].as_str().is_some_and(|kind| kind.starts_with("text/")))
        .and_then(|data| base64::engine::general_purpose::URL_SAFE.decode(data).ok())
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .unwrap_or_default();
    let parts = part["parts"].as_array().into_iter().flatten().map(body);
    std::iter::once(own).chain(parts).collect::<Vec<_>>().join(" ")
}

/// The first number of exactly six digits that stands on its own.
fn code_in(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let digit = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_digit);
    (0..bytes.len().saturating_sub(5))
        .find(|&at| {
            (at..at + 6).all(digit) && !digit(at + 6) && (at == 0 || !digit(at - 1)) && !text[..at].ends_with('#')
        })
        .map(|at| text[at..at + 6].to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_code_is_the_lone_six_digit_number() {
        assert_eq!(
            code_in("Your verification code is 482913. It expires in 10 minutes."),
            Some("482913".into())
        );
        assert_eq!(code_in("Order 12345678 shipped"), None);
        assert_eq!(code_in("Use 004211 to sign in"), Some("004211".into()));
    }

    #[test]
    fn a_mail_from_before_the_sign_in_is_not_read() {
        let message = json!({ "internalDate": "1000", "snippet": "code 123456" });
        assert_eq!(code_after(&message, Timestamp::from_millisecond(2000).unwrap()), None);
        assert_eq!(
            code_after(&message, Timestamp::from_millisecond(500).unwrap()),
            Some("123456".into())
        );
    }
}
