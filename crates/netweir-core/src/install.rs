//! `netweir install chrome`: the Chrome for Testing build that matches
//! netweir's newest Chrome profile, unpacked where `find_chrome` looks.

use std::path::{Path, PathBuf};
use std::time::Duration;

use netweir_browser::{cft_executable, cft_platform};
use serde_json::Value;

use crate::fetch::{FetchOptions, Fetcher};
use crate::profile::{DEFAULT, Profile};

/// Google's list of the newest build of each Chrome milestone.
pub const CHROME_FOR_TESTING: &str = "https://googlechromelabs.github.io/chrome-for-testing/latest-versions-per-milestone-with-downloads.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub version: String,
    pub executable: PathBuf,
    /// It was there already; nothing was downloaded.
    pub already: bool,
}

/// Installs the Chrome for Testing build (or, with `headless_shell`,
/// `chrome-headless-shell`) of netweir's newest Chrome profile's
/// milestone, as `index` lists it, into `home/chrome/<version>/`. A build
/// already there isn't downloaded again.
pub async fn install_chrome(
    index: &str,
    home: &Path,
    headless_shell: bool,
) -> Result<Installed, String> {
    let platform = cft_platform().ok_or_else(|| {
        format!(
            "no Chrome for Testing builds for {} on {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let milestone = DEFAULT
        .split('-')
        .nth(1)
        .ok_or("netweir's default profile has no version in its name")?;
    let mut options = FetchOptions::new(Profile::named("chrome").map_err(|e| e.to_string())?);
    // A download is a few hundred megabytes.
    options.timeout = Duration::from_secs(600);
    let fetcher = Fetcher::new(options).map_err(|e| e.message)?;

    let listing = fetcher
        .get(index, &[])
        .await
        .map_err(|e| format!("{index}: {}", e.message))?;
    if listing.status != 200 {
        return Err(format!("{index}: HTTP {}", listing.status));
    }
    let listing: Value =
        serde_json::from_slice(&listing.body).map_err(|e| format!("{index}: {e}"))?;
    let build = &listing["milestones"][milestone];
    let version = build["version"]
        .as_str()
        .ok_or_else(|| format!("{index} has no Chrome {milestone}"))?
        .to_string();
    let kind = if headless_shell {
        "chrome-headless-shell"
    } else {
        "chrome"
    };
    let downloads = build["downloads"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let url = downloads
        .iter()
        .find(|d| d["platform"] == platform)
        .and_then(|d| d["url"].as_str())
        .ok_or_else(|| {
            let there: Vec<&str> = downloads
                .iter()
                .filter_map(|d| d["platform"].as_str())
                .collect();
            format!(
                "no Chrome for Testing {kind} {version} for {platform}; there is for: {}",
                if there.is_empty() {
                    "nothing".to_string()
                } else {
                    there.join(", ")
                }
            )
        })?
        .to_string();

    let dir = home.join("chrome").join(&version);
    let executable = dir.join(cft_executable(platform, headless_shell));
    if executable.is_file() {
        return Ok(Installed {
            version,
            executable,
            already: true,
        });
    }
    let zip = fetcher
        .get(&url, &[])
        .await
        .map_err(|e| format!("{url}: {}", e.message))?;
    if zip.status != 200 {
        return Err(format!("{url}: HTTP {}", zip.status));
    }
    // Unpacked beside its final place and moved in whole, so a download
    // cut short never looks installed.
    let partial = home
        .join("chrome")
        .join(format!(".{version}-{kind}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&partial);
    let unpacked = tokio::task::spawn_blocking({
        let partial = partial.clone();
        move || -> Result<(), String> {
            std::fs::create_dir_all(&partial).map_err(|e| e.to_string())?;
            let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip.body))
                .map_err(|e| format!("not a zip: {e}"))?;
            archive.extract(&partial).map_err(|e| e.to_string())
        }
    })
    .await
    .map_err(|e| e.to_string())
    .and_then(|r| r);
    let moved = unpacked.and_then(|()| {
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(&partial).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            let to = dir.join(entry.file_name());
            if !to.exists() {
                std::fs::rename(entry.path(), &to).map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    });
    let _ = std::fs::remove_dir_all(&partial);
    moved.map_err(|e| format!("unpacking {url} into {}: {e}", dir.display()))?;
    if !executable.is_file() {
        return Err(format!("{url} didn't hold {}", executable.display()));
    }
    Ok(Installed {
        version,
        executable,
        already: false,
    })
}
