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
