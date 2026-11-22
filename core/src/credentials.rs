//! Reads a sign-in, an account name and its password, from the macOS Keychain.
//!
//! It runs `/usr/bin/security`, which every Mac has, so no crate is needed. The user stores an
//! item with the same command, and macOS lets a command read the items it created without asking.
//!
//! A password never goes into an error, a log or a `Debug` output. See `Password`.

use std::fmt;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

const SECURITY: &str = "/usr/bin/security";
/// What `security` exits with when the Keychain holds no such item.
const NOT_FOUND: i32 = 44;
/// macOS can ask the user to allow the read, and the command waits for the answer.
const TIME_LIMIT: Duration = Duration::from_secs(60);

/// A password. It is its own type so that printing a value that holds one shows nothing.
#[derive(Clone, PartialEq)]
pub struct Password(String);

impl Password {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The password itself. Only the code that types it into a page asks for this.
    pub fn reveal(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(hidden)")
    }
}

/// An account name and its password.
#[derive(Clone, Debug, PartialEq)]
pub struct SignIn {
    pub account: String,
    pub password: Password,
}

/// Where a sign-in comes from. The app asks the Keychain. A test hands over made-up values.
pub trait SignIns {
    /// The sign-in stored for `service`, or `None` when there is none.
    /// An error says why it could not be read, and never holds a password.
    fn sign_in(&self, service: &str) -> impl Future<Output = Result<Option<SignIn>, String>>;
}

/// The user's login Keychain.
pub struct Keychain;

impl SignIns for Keychain {
    async fn sign_in(&self, service: &str) -> Result<Option<SignIn>, String> {
        read(service).await
    }
}

/// A fixed answer: this sign-in, or none at all.
impl SignIns for Option<SignIn> {
    async fn sign_in(&self, _service: &str) -> Result<Option<SignIn>, String> {
        Ok(self.clone())
    }
}

/// Reads the generic password item stored for `service`.
async fn read(service: &str) -> Result<Option<SignIn>, String> {
    // `-g` prints the password to the error output and the item's attributes, the account name
    // among them, to the standard output.
    let run = Command::new(SECURITY)
        .args(["find-generic-password", "-s", service, "-g"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIME_LIMIT, run)
        .await
        .map_err(|_| "the Keychain did not answer in time".to_owned())?
        .map_err(|error| format!("could not run {SECURITY}: {error}"))?;
    if output.status.code() == Some(NOT_FOUND) {
        return Ok(None);
    }
    let said = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        // A failed read prints no password. A line that looks like one is still kept out.
        let why = said
            .lines()
            .find(|line| !line.starts_with(PASSWORD_LINE))
            .unwrap_or_default();
        return Err(format!("{SECURITY} failed: {}", why.trim()));
    }
    parse(&String::from_utf8_lossy(&output.stdout), &said).map(Some)
}

/// The password stored for `account` under `service`, or `None` when there is none.
/// Use this when one service holds several accounts.
pub async fn secret(service: &str, account: &str) -> Result<Option<Password>, String> {
    let run = Command::new(SECURITY)
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIME_LIMIT, run)
        .await
        .map_err(|_| "the Keychain did not answer in time".to_owned())?
        .map_err(|error| format!("could not run {SECURITY}: {error}"))?;
    if output.status.code() == Some(NOT_FOUND) {
        return Ok(None);
    }
    if !output.status.success() {
        return Err(format!(
            "{SECURITY} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let printed = String::from_utf8_lossy(&output.stdout);
    let password = printed.strip_suffix('\n').unwrap_or(&printed);
    if password.is_empty() {
        return Err("the Keychain item's password is empty".to_owned());
    }
    Ok(Some(Password::new(password)))
}

/// Stores `password` for `account` under `service`, replacing any password stored there.
///
/// The command reads the password from its input, so it never shows in the list of processes.
/// A value with a quote, a backslash or a line break is refused rather than escaped.
pub async fn store(service: &str, account: &str, password: &Password) -> Result<(), String> {
    let plain = |text: &str| !text.is_empty() && !text.contains(['"', '\\', '\n', '\r']);
    if !(plain(service) && plain(account) && plain(password.reveal())) {
        return Err("a Keychain value may not be empty or hold a quote, a backslash or a line break".to_owned());
    }
    let command = format!(
        "add-generic-password -U -s \"{service}\" -a \"{account}\" -w \"{}\"\n",
        password.reveal()
    );
    run_interactive(&command).await
}

/// Deletes the item for `account` under `service`. A missing item is not an error.
pub async fn forget(service: &str, account: &str) -> Result<(), String> {
    let run = Command::new(SECURITY)
        .args(["delete-generic-password", "-s", service, "-a", account])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(TIME_LIMIT, run)
        .await
        .map_err(|_| "the Keychain did not answer in time".to_owned())?
        .map_err(|error| format!("could not run {SECURITY}: {error}"))?;
    match output.status.code() {
        Some(0) | Some(NOT_FOUND) => Ok(()),
        _ => Err(format!(
            "{SECURITY} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
    }
}

/// Runs one `security` command read from standard input.
async fn run_interactive(command: &str) -> Result<(), String> {
    use tokio::io::AsyncWriteExt as _;

    let mut child = Command::new(SECURITY)
        .arg("-i")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run {SECURITY}: {error}"))?;
    let mut input = child.stdin.take().expect("the input is piped");
    input
        .write_all(command.as_bytes())
        .await
        .map_err(|error| format!("could not talk to {SECURITY}: {error}"))?;
    drop(input);
    let output = tokio::time::timeout(TIME_LIMIT, child.wait_with_output())
        .await
        .map_err(|_| "the Keychain did not answer in time".to_owned())?
        .map_err(|error| format!("{SECURITY} failed: {error}"))?;
    let said = String::from_utf8_lossy(&output.stderr);
    // `security -i` exits 0 even when its command fails, and reports the failure on its error output.
    if !output.status.success() || !said.trim().is_empty() {
        return Err(format!("{SECURITY} failed: {}", said.trim()));
    }
    Ok(())
}

const PASSWORD_LINE: &str = "password: ";
const ACCOUNT_LINE: &str = "\"acct\"<blob>=";

/// Reads the account name out of the attributes `security` printed, and the password out of its
/// password line.
fn parse(attributes: &str, password_output: &str) -> Result<SignIn, String> {
    let account = attributes
        .lines()
        .find_map(|line| line.trim_start().strip_prefix(ACCOUNT_LINE))
        .and_then(value)
        .filter(|account| !account.is_empty())
        .ok_or("the Keychain item has no account name")?;
    let password = password_output
        .lines()
        .find_map(|line| line.strip_prefix(PASSWORD_LINE))
        .and_then(value)
        .filter(|password| !password.is_empty())
        .ok_or("the Keychain item's password is empty or could not be read")?;
    Ok(SignIn {
        account,
        password: Password::new(password),
    })
}

/// One value as `security` prints it. It has two forms:
///
/// - `"text"` for plain text. A quote inside the text is printed as it is, so the value runs from
///   the first quote to the last one.
/// - `0x70C3A9  "p\303\251"` for text with a backslash, a control character or a character
///   outside ASCII: the bytes in hexadecimal, then a copy for people that this code ignores.
///
/// A missing value prints as `<NULL>` or as nothing, and gives `None`.
fn value(printed: &str) -> Option<String> {
    if let Some(hexadecimal) = printed.strip_prefix("0x") {
        let digits = hexadecimal.split_whitespace().next()?.as_bytes();
        if digits.len() % 2 != 0 {
            return None;
        }
        let digit = |byte: u8| char::from(byte).to_digit(16);
        let bytes: Option<Vec<u8>> = digits
            .chunks(2)
            .map(|pair| Some((digit(pair[0])? * 16 + digit(pair[1])?) as u8))
            .collect();
        return String::from_utf8(bytes?).ok();
    }
    let text = printed.strip_prefix('"')?.strip_suffix('"')?;
    Some(text.to_owned())
}

#[cfg(test)]
mod tests {
    use std::process::Command as Process;

    use super::*;

    /// A made-up password with a quote, a backslash, a space and a character outside ASCII.
    const AWKWARD: &str = "Made-up 1 \"pass\\word\" é";

    fn attributes(account: &str) -> String {
        format!(
            "keychain: \"/Users/ada/Library/Keychains/login.keychain-db\"\nversion: 512\nclass: \"genp\"\nattributes:\n    \
             0x00000007 <blob>=\"dev.maverick.test\"\n    \"acct\"<blob>={account}\n    \"desc\"<blob>=<NULL>\n    \
             \"svce\"<blob>=\"dev.maverick.test\"\n"
        )
    }

    fn parsed(account: &str, password_line: &str) -> Result<(String, String), String> {
        parse(&attributes(account), password_line)
            .map(|sign_in| (sign_in.account, sign_in.password.reveal().to_owned()))
    }

    #[test]
    fn a_plain_item_is_read_between_its_first_and_last_quote() {
        assert_eq!(
            parsed("\"ada@example.com\"", "password: \"plain-Pass1!\"\n"),
            Ok(("ada@example.com".into(), "plain-Pass1!".into()))
        );
        // `security` prints a quote inside plain text as it is.
        assert_eq!(
            parsed("\"a\"b@example.com\"", "password: \"with\"quote\"\"\n"),
            Ok(("a\"b@example.com".into(), "with\"quote\"".into()))
        );
        // Plain text that starts like the hexadecimal form still has its quotes.
        assert_eq!(
            parsed("\"ada@example.com\"", "password: \"0x41  \"B\"\"\n").unwrap().1,
            "0x41  \"B\""
        );
    }

    #[test]
    fn an_item_printed_in_hexadecimal_is_read_from_its_bytes() {
        // What `security` printed for these values on macOS 26.
        assert_eq!(
            parsed(
                "0x6164C3A9406578616D706C652E636F6D  \"ad\\303\\251@example.com\"",
                "password: 0x70612273735C77206F726420C3A9  \"pa\"ss\\134w ord \\303\\251\"\n",
            ),
            Ok(("adé@example.com".into(), "pa\"ss\\w ord é".into()))
        );
        assert_eq!(value("0x74616209696e  \"tab\\011in\"").as_deref(), Some("tab\tin"));
    }

    #[test]
    fn an_item_without_an_account_or_a_password_is_an_error_that_shows_no_password() {
        let no_account = parsed("<NULL>", "password: \"secret-Pass1!\"\n").unwrap_err();
        assert!(no_account.contains("no account name"), "{no_account}");
        assert!(!no_account.contains("secret"), "{no_account}");
        for unreadable in [
            "password: \n",
            "",
            "password: 0x7  \"x\"\n",
            "password: 0xFF  \"\\377\"\n",
        ] {
            let problem = parsed("\"ada@example.com\"", unreadable).unwrap_err();
            assert!(problem.contains("password is empty or could not be read"), "{problem}");
        }
    }

    #[test]
    fn printing_a_sign_in_shows_the_account_and_hides_the_password() {
        let sign_in = SignIn {
            account: "ada@example.com".into(),
            password: Password::new(AWKWARD),
        };
        let printed = format!("{sign_in:?} {:#?}", Some(&sign_in));
        assert!(
            printed.contains("ada@example.com") && printed.contains("hidden"),
            "{printed}"
        );
        assert!(!printed.contains("Made-up") && !printed.contains('é'), "{printed}");
    }

    /// A made-up item in the login Keychain, under a service name no other run uses. It is
    /// deleted when the test ends, however it ends.
    struct TestItem(String);

    impl TestItem {
        fn store(account: &str, password: &str) -> Self {
            let nanos = std::time::UNIX_EPOCH.elapsed().unwrap().subsec_nanos();
            let item = Self(format!("dev.maverick.test-{}-{nanos}", std::process::id()));
            let added = Process::new(SECURITY)
                .args(["add-generic-password", "-s", &item.0, "-a", account, "-w", password])
                .status()
                .unwrap();
            assert!(added.success(), "the test item was stored");
            item
        }
    }

    impl Drop for TestItem {
        fn drop(&mut self) {
            let _ = Process::new(SECURITY)
                .args(["delete-generic-password", "-s", &self.0])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    /// Stores, reads, replaces and deletes made-up items in the real login Keychain.
    #[tokio::test]
    async fn a_service_holds_one_secret_per_account() {
        let nanos = std::time::UNIX_EPOCH.elapsed().unwrap().subsec_nanos();
        let service = format!("dev.maverick.test-{}-{nanos}", std::process::id());
        let made_up = |text: &str| Password::new(text);
        store(&service, "ada@example.com", &made_up("1//first-token_A"))
            .await
            .unwrap();
        store(&service, "bo@example.com", &made_up("1//second-token_B"))
            .await
            .unwrap();
        store(&service, "ada@example.com", &made_up("1//replaced-token"))
            .await
            .unwrap();

        let read = |account: &'static str| {
            let service = service.clone();
            async move { secret(&service, account).await.unwrap().map(|p| p.reveal().to_owned()) }
        };
        assert_eq!(read("ada@example.com").await.as_deref(), Some("1//replaced-token"));
        assert_eq!(read("bo@example.com").await.as_deref(), Some("1//second-token_B"));
        assert_eq!(read("cy@example.com").await, None);

        forget(&service, "ada@example.com").await.unwrap();
        forget(&service, "ada@example.com").await.unwrap();
        assert_eq!(read("ada@example.com").await, None);
        forget(&service, "bo@example.com").await.unwrap();

        let refused = store(&service, "ada@example.com", &made_up("has \"quote\"")).await;
        assert!(refused.is_err());
    }

    /// Reads the real login Keychain, but only items this test stores itself, with made-up values.
    #[tokio::test]
    async fn the_keychain_gives_back_an_awkward_password_and_none_for_a_missing_item() {
        let item = TestItem::store("ada@example.com", AWKWARD);
        let read = Keychain.sign_in(&item.0).await.unwrap().expect("the item is there");
        assert_eq!(read.account, "ada@example.com");
        assert_eq!(read.password.reveal(), AWKWARD);
        let service = item.0.clone();
        drop(item);
        assert_eq!(Keychain.sign_in(&service).await, Ok(None));

        let plain = TestItem::store("adé \"quoted\"@example.com", "plain-Pass1!");
        let read = Keychain.sign_in(&plain.0).await.unwrap().expect("the item is there");
        assert_eq!(read.account, "adé \"quoted\"@example.com");
        assert_eq!(read.password.reveal(), "plain-Pass1!");
    }
}
