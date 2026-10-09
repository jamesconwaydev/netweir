//! Finding Chrome and starting it with the DevTools pipe.

use std::io::{PipeReader, PipeWriter};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{Error, Result};

/// The Chrome to launch: `$NETWEIR_CHROME`, then the usual install paths
/// (Chrome, Chrome for Testing, Chromium). The error lists where it
/// looked.
pub fn find_chrome() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("NETWEIR_CHROME") {
        let path = PathBuf::from(path);
        return if path.is_file() {
            Ok(path)
        } else {
            Err(Error::Launch(format!(
                "$NETWEIR_CHROME is {}, which doesn't exist",
                path.display()
            )))
        };
    }
    let candidates = candidates();
    candidates
        .iter()
        .find(|p| p.is_file())
        .cloned()
        .ok_or_else(|| {
            let looked: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
            Error::Launch(format!(
                "no Chrome found; set $NETWEIR_CHROME or pass executable=. Looked in: {}",
                looked.join(", ")
            ))
        })
}

#[cfg(target_os = "macos")]
fn candidates() -> Vec<PathBuf> {
    let apps = [
        "Google Chrome.app/Contents/MacOS/Google Chrome",
        "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        "Chromium.app/Contents/MacOS/Chromium",
    ];
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join("Applications"));
    }
    dirs.iter()
        .flat_map(|d| apps.iter().map(move |a| d.join(a)))
        .collect()
}

#[cfg(all(unix, not(target_os = "macos")))]
fn candidates() -> Vec<PathBuf> {
    let names = [
        "google-chrome",
        "google-chrome-stable",
        "chrome",
        "chromium",
        "chromium-browser",
    ];
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut found: Vec<PathBuf> = std::env::split_paths(&path)
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .collect();
    found.push(PathBuf::from("/opt/google/chrome/chrome"));
    found
}

#[cfg(windows)]
fn candidates() -> Vec<PathBuf> {
    let suffixes = [
        r"Google\Chrome\Application\chrome.exe",
        r"Google\Chrome for Testing\Application\chrome.exe",
        r"Chromium\Application\chrome.exe",
    ];
    ["ProgramFiles", "ProgramFiles(x86)", "LocalAppData"]
        .iter()
        .filter_map(std::env::var_os)
        .flat_map(|d| suffixes.iter().map(move |s| Path::new(&d).join(s)))
        .collect()
}

/// A started Chrome: the process, and the pipe's two ends on our side.
pub(crate) struct Started {
    pub child: Child,
    pub commands: PipeWriter,
    pub replies: PipeReader,
}

pub(crate) fn start(executable: &Path, args: &[String]) -> Result<Started> {
    if !executable.is_file() {
        return Err(Error::Launch(format!(
            "no Chrome at {}",
            executable.display()
        )));
    }
    let failed =
        |e: std::io::Error| Error::Launch(format!("can't start {}: {e}", executable.display()));
    // Chrome reads commands from one pipe and writes replies to the other.
    let (chrome_reads, commands) = std::io::pipe().map_err(failed)?;
    let (replies, chrome_writes) = std::io::pipe().map_err(failed)?;
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hand_over(&mut command, &chrome_reads, &chrome_writes).map_err(failed)?;
    let child = command.spawn().map_err(failed)?;
    // Our copies of Chrome's ends must close, or we'd never see it exit.
    drop((chrome_reads, chrome_writes));
    Ok(Started {
        child,
        commands,
        replies,
    })
}

/// Gives Chrome its ends of the pipe as file descriptors 3 and 4, where
/// `--remote-debugging-pipe` expects them.
#[cfg(unix)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    command.arg("--remote-debugging-pipe");
    let (reads, writes) = (reads.as_raw_fd(), writes.as_raw_fd());
    // SAFETY: only async-signal-safe calls (fcntl, dup2) between fork and
    // exec.
    unsafe {
        command.pre_exec(move || {
            // Either end may already be 3 or 4; move both out of the way
            // first so the dup2s can't clobber each other. dup2 clears
            // close-on-exec on 3 and 4; the moved copies keep it.
            let reads = libc::fcntl(reads, libc::F_DUPFD_CLOEXEC, 5);
            let writes = libc::fcntl(writes, libc::F_DUPFD_CLOEXEC, 5);
            if reads < 0 || writes < 0 || libc::dup2(reads, 3) < 0 || libc::dup2(writes, 4) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

/// Windows has no numbered descriptors to hand over: the pipe handles are
/// made inheritable and their values passed on the command line.
#[cfg(windows)]
fn hand_over(
    command: &mut Command,
    reads: &PipeReader,
    writes: &PipeWriter,
) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};

    // ponytail: every inheritable handle goes to every child started
    // meanwhile, so two browsers launched at once may each hold the
    // other's pipe ends. Harmless while both live; a job object or
    // PROC_THREAD_ATTRIBUTE_HANDLE_LIST would make it exact.
    for handle in [reads.as_raw_handle(), writes.as_raw_handle()] {
        if unsafe { SetHandleInformation(handle as _, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) }
            == 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    command.arg("--remote-debugging-pipe").arg(format!(
        "--remote-debugging-io-pipes={},{}",
        reads.as_raw_handle() as usize,
        writes.as_raw_handle() as usize
    ));
    Ok(())
}
