//! Looking for a newer release on GitHub, in the background: versions are
//! 0.<number of commits>.0 and releases are tagged v0.<n>.0.

use std::sync::mpsc::{Receiver, channel};

/// Where releases are published.
const LATEST: &str = "https://api.github.com/repos/roflox/RofLoxBox/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/roflox/RofLoxBox/releases/latest";

#[derive(Debug, Clone, PartialEq)]
pub struct Release {
    /// "0.36.0"
    pub version: String,
    /// The release's page.
    pub url: String,
}

/// What the last check found.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Newer(Release),
    UpToDate(String),
    Failed(String),
}

#[derive(Default)]
pub struct UpdateCheck {
    running: Option<Receiver<Result<Release, String>>>,
    pub outcome: Option<Outcome>,
    /// The newer release the top bar offers (until the user dismisses it).
    pub offer: Option<Release>,
}

impl UpdateCheck {
    /// Starts a check unless one is running.
    pub fn start(&mut self) {
        if self.running.is_some() {
            return;
        }
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(fetch_latest());
        });
        self.running = Some(rx);
    }

    pub fn checking(&self) -> bool {
        self.running.is_some()
    }

    /// The outcome once a check finishes (once).
    pub fn poll(&mut self, current: &str) -> Option<&Outcome> {
        let result = self.running.as_ref()?.try_recv().ok()?;
        self.running = None;
        let outcome = match result {
            Ok(release) if is_newer(&release.version, current) => {
                self.offer = Some(release.clone());
                Outcome::Newer(release)
            }
            Ok(release) => Outcome::UpToDate(release.version),
            Err(err) => Outcome::Failed(err),
        };
        self.outcome = Some(outcome);
        self.outcome.as_ref()
    }
}

/// The latest release, asked of GitHub with curl (macOS, Windows 10 and
/// later, and Linux have it). Only the app's name goes along.
fn fetch_latest() -> Result<Release, String> {
    let output = crate::stems::no_window(&mut std::process::Command::new("curl"))
        .args(["-fsSL", "--max-time", "10", "-A", crate::APP_NAME])
        .args(["-H", "Accept: application/vnd.github+json", LATEST])
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("needs curl: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("unreadable answer: {e}"))?;
    let tag = json["tag_name"].as_str().ok_or("no release found")?;
    let url = json["html_url"].as_str().unwrap_or(RELEASES_PAGE);
    Ok(Release { version: tag.trim_start_matches('v').to_owned(), url: url.to_owned() })
}

/// The commit count in a version: 0.<n>.0, or 0.<n>.0-dev (what an
/// uncommitted build will become).
fn number(version: &str) -> Option<u64> {
    version.trim_start_matches('v').split('.').nth(1)?.parse().ok()
}

/// Whether `latest` is newer than `current`.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (number(latest), number(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_commit_count() {
        assert!(is_newer("0.36.0", "0.35.0"));
        assert!(is_newer("v0.100.0", "0.99.0"));
        assert!(!is_newer("0.35.0", "0.35.0"));
        // A -dev build is the version it will become.
        assert!(!is_newer("0.36.0", "0.36.0-dev"));
        assert!(is_newer("0.37.0", "0.36.0-dev"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
