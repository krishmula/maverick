//! Calls to web APIs through `/usr/bin/curl`, with the token passed on its input so no token
//! ever shows in the list of processes.

use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::credentials::Password;

const TIME_LIMIT: Duration = Duration::from_secs(60);

/// One authorized call to `service`. `body` is sent as JSON. The reply is JSON; an empty reply is `null`.
pub async fn call(
    service: &str,
    token: &Password,
    method: &str,
    url: &str,
    body: Option<&Value>,
) -> Result<Value, String> {
    let mut config = format!(
        "url = {}\nrequest = {}\nheader = {}\n",
        quoted(url),
        quoted(method),
        quoted(&format!("Authorization: Bearer {}", token.reveal()))
    );
    if let Some(body) = body {
        config.push_str(&format!(
            "header = \"Content-Type: application/json\"\ndata-binary = {}\n",
            quoted(&body.to_string())
        ));
    }
    let (status, reply) = curl(&config).await?;
    let json: Value = if reply.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&reply).map_err(|_| format!("{service} answered with status {status} and no JSON"))?
    };
    if !(200..300).contains(&status) {
        let message = json["error"]["message"]
            .as_str()
            .or(json["error_description"].as_str())
            .or(json["errorMessage"].as_str())
            .unwrap_or("no reason given");
        return Err(format!("{service} answered with status {status}: {message}"));
    }
    Ok(json)
}

/// Runs curl with its options on standard input, and returns the status and the body.
pub(crate) async fn curl(config: &str) -> Result<(u16, String), String> {
    let mut child = Command::new("/usr/bin/curl")
        .args(["--silent", "--show-error", "--config", "-"])
        .args([
            "--max-time",
            &TIME_LIMIT.as_secs().to_string(),
            "--write-out",
            "\n%{http_code}",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run curl: {error}"))?;
    let mut input = child.stdin.take().expect("the input is piped");
    input
        .write_all(config.as_bytes())
        .await
        .map_err(|error| format!("could not talk to curl: {error}"))?;
    drop(input);
    let output = child
        .wait_with_output()
        .await
        .map_err(|error| format!("curl failed: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "the request failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let (body, status) = text.rsplit_once('\n').ok_or("curl gave no status")?;
    Ok((
        status.trim().parse().map_err(|_| "curl gave no status")?,
        body.to_owned(),
    ))
}

/// A value for a curl config file: in double quotes, with `\` and `"` escaped and no line breaks.
pub(crate) fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub(crate) fn form_encoded(text: &str) -> String {
    let mut out = String::new();
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_values_and_form_fields_are_escaped() {
        assert_eq!(quoted(r#"a "b" \c"#), r#""a \"b\" \\c""#);
        assert_eq!(quoted("x\ny"), r#""x\ny""#);
        assert_eq!(form_encoded("1//a b&c=d"), "1%2F%2Fa%20b%26c%3Dd");
    }
}
