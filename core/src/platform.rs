//! The application platform a posting lives on, as the user knows it.

use serde::{Deserialize, Serialize};

use crate::apply::{self, System};
use crate::jobs::{self, Posting};

/// Where a posting takes applications. Maverick fills forms on the first three; the rest open for the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Platform {
    Ashby,
    Greenhouse,
    Workday,
    Lever,
    AmazonJobs,
    OracleCloud,
    Simplify,
    LinkedIn,
    CompanySite,
}

impl Platform {
    pub const ALL: [Self; 9] = [
        Self::Ashby,
        Self::Greenhouse,
        Self::Workday,
        Self::Lever,
        Self::AmazonJobs,
        Self::OracleCloud,
        Self::Simplify,
        Self::LinkedIn,
        Self::CompanySite,
    ];

    /// The platform of the posting at `address`.
    pub fn of(address: &str) -> Self {
        if let Some(system) = System::of(address) {
            return match system {
                System::Ashby => Self::Ashby,
                System::Greenhouse => Self::Greenhouse,
                System::Workday => Self::Workday,
            };
        }
        let host = host(address);
        let on = |domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
        if on("simplify.jobs") {
            Self::Simplify
        } else if on("amazon.jobs") {
            Self::AmazonJobs
        } else if on("oraclecloud.com") {
            Self::OracleCloud
        } else if on("lever.co") {
            Self::Lever
        } else if on("linkedin.com") {
            Self::LinkedIn
        } else {
            Self::CompanySite
        }
    }

    /// Whether Maverick fills and submits forms here by itself.
    pub fn fillable(self) -> bool {
        matches!(self, Self::Ashby | Self::Greenhouse | Self::Workday)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Ashby => "Ashby",
            Self::Greenhouse => "Greenhouse",
            Self::Workday => "Workday",
            Self::Lever => "Lever",
            Self::AmazonJobs => "Amazon Jobs",
            Self::OracleCloud => "Oracle Cloud",
            Self::Simplify => "Simplify",
            Self::LinkedIn => "LinkedIn",
            Self::CompanySite => "Company site",
        }
    }
}

fn host(address: &str) -> &str {
    let rest = address.split_once("://").map_or(address, |(_, rest)| rest);
    rest.split(['/', '?', '#']).next().unwrap_or(rest)
}

/// The posting's own address: the one scoring found, else the one a job board's link wraps,
/// without anything a tracker or a campaign added.
pub fn address_of(posting: &Posting) -> String {
    let address = posting
        .address
        .clone()
        .unwrap_or_else(|| jobs::posting_address(&posting.link));
    apply::posting_address(&address).unwrap_or(address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_address_names_its_platform() {
        for (address, platform) in [
            (
                "https://jobs.ashbyhq.com/example/0f3b8231-3a32-478a-b994-3441f4bb9401",
                Platform::Ashby,
            ),
            (
                "https://job-boards.greenhouse.io/example/jobs/123",
                Platform::Greenhouse,
            ),
            ("https://www.amazon.jobs/en/jobs/123/engineer", Platform::AmazonJobs),
            (
                "https://example.fa.us2.oraclecloud.com/hcmUI/job/1",
                Platform::OracleCloud,
            ),
            ("https://jobs.lever.co/example/1", Platform::Lever),
            ("https://simplify.jobs/p/9", Platform::Simplify),
            ("https://careers.example.com/jobs/1", Platform::CompanySite),
        ] {
            assert_eq!(Platform::of(address), platform, "{address}");
        }
    }
}
