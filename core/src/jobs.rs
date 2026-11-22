//! The records of the job funnel: postings found, applications sent, and hiring posts.

use jiff::Timestamp;
use jiff::civil::Date;

/// Declares an enum whose variants each have one stored name.
/// `as_str` and `parse` come from the same list, so they cannot disagree.
macro_rules! stored_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub const ALL: &[$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }

            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some($name::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

stored_enum! {
    /// Where a posting is in the funnel.
    Stage {
        Found => "found",
        Duplicate => "duplicate",
        Skipped => "skipped",
        Review => "review",
        Applied => "applied",
    }
}

stored_enum! {
    ApplicationStatus {
        Pending => "pending",
        Rejected => "rejected",
    }
}

stored_enum! {
    /// How far outreach to the author of a hiring post has gone.
    Outreach {
        NotStarted => "not_started",
        Skipped => "skipped",
        Drafted => "drafted",
        Sent => "sent",
        Replied => "replied",
    }
}

/// A job posting that intake found. `link` is the address as intake received it, and it identifies the posting.
#[derive(Clone, Debug, PartialEq)]
pub struct Posting {
    pub link: String,
    pub found_at: Timestamp,
    pub source: String,
    pub company: String,
    pub title: String,
    pub location: Option<String>,
    pub email_subject: Option<String>,
    pub stage: Stage,
    pub score: Option<u8>,
    pub variant: Option<String>,
    pub flags: Option<String>,
    pub notes: Option<String>,
}

/// An application that was submitted.
#[derive(Clone, Debug, PartialEq)]
pub struct Application {
    /// The row in the tracker sheet. It identifies an application that came from the sheet.
    pub sheet_row: u32,
    /// Company and role as one line. The sheet export can lose it, so it may be absent.
    pub role: Option<String>,
    pub link: Option<String>,
    pub applied_on: Date,
    pub status: ApplicationStatus,
    pub notes: Option<String>,
}

/// A post where someone says they are hiring. `post_link` and `role` together identify it.
#[derive(Clone, Debug, PartialEq)]
pub struct HiringPost {
    pub post_link: String,
    pub role: String,
    pub found_at: Timestamp,
    pub poster: String,
    pub company: String,
    pub score: u8,
    pub variant: String,
    pub outreach: Outreach,
    pub notes: Option<String>,
}

/// The posting's own address. Links from Notify wrap it, URL-encoded, as one part of their path.
pub fn posting_address(link: &str) -> String {
    let wrapped = link
        .strip_prefix("https://links.notify.careers/CL0/")
        .and_then(|rest| rest.split('/').next());
    match wrapped {
        Some(encoded) => percent_decoded(encoded),
        None => link.to_owned(),
    }
}

fn percent_decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escaped = (bytes[at] == b'%')
            .then(|| text.get(at + 1..at + 3))
            .flatten()
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                decoded.push(byte);
                at += 3;
            }
            None => {
                decoded.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_notify_link_gives_the_posting_it_wraps() {
        assert_eq!(
            posting_address(
                "https://links.notify.careers/CL0/https:%2F%2Fjobs.example.com%2Fco%2F42%3Fa=b%20c/1/0111-000/x=258"
            ),
            "https://jobs.example.com/co/42?a=b c"
        );
        assert_eq!(
            posting_address("https://jobs.example.com/co/42"),
            "https://jobs.example.com/co/42"
        );
        assert_eq!(percent_decoded("100%25 %zz %4"), "100% %zz %4");
    }
}
