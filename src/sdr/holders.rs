//! Best-effort detection of programs that may be holding the RTL-SDR open,
//! so an `in_use` test result can name the likely culprit.
// Parts are only reached from the Windows build; they are tested everywhere.
#![cfg_attr(not(windows), allow(dead_code))]

/// Process-name fragment (lowercase) and the label shown to the user.
const PATTERNS: &[(&str, &str)] = &[
    ("sdrsharp", "SDR#/SDRSharp"),
    ("sdr#", "SDR#/SDRSharp"),
    ("sdrpp", "SDR++"),
    ("sdr++", "SDR++"),
    ("spyserver", "SpyServer"),
    ("rtl_tcp", "rtl_tcp"),
    ("rtl_fm", "rtl_fm"),
    ("rtl_433", "rtl_433"),
    ("rtl_power", "rtl_power"),
    ("hdsdr", "HDSDR"),
    ("dump1090", "dump1090"),
    ("gqrx", "Gqrx"),
    ("sdrangel", "SDRangel"),
    ("cubicsdr", "CubicSDR"),
];

/// What `holders` contains when nothing recognisable is running: the usual
/// suspects, so the message still tells the user where to look.
pub const LIKELY_HOLDERS: [&str; 7] = [
    "SDR#/SDRSharp",
    "SDR++",
    "SpyServer",
    "rtl_tcp",
    "HDSDR",
    "dump1090",
    "another azimuth-daemon",
];

/// Match running processes `(pid, name)` against the known SDR programs.
/// Another `azimuth-daemon` counts unless it is this process.
pub fn holders_from_processes(procs: &[(u32, String)], own_pid: u32) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (pid, name) in procs {
        let lower = name.to_ascii_lowercase();
        let label = if lower.contains("azimuth-daemon") {
            if *pid == own_pid {
                continue;
            }
            Some("another azimuth-daemon")
        } else {
            PATTERNS.iter().find(|(p, _)| lower.contains(p)).map(|(_, l)| *l)
        };
        if let Some(l) = label {
            if !out.iter().any(|x| x == l) {
                out.push(l.to_string());
            }
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn list_processes() -> Option<Vec<(u32, String)>> {
    let mut out = Vec::new();
    let dir = std::fs::read_dir("/proc").ok()?;
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(comm) = std::fs::read_to_string(entry.path().join("comm")) {
            out.push((pid, comm.trim().to_string()));
        }
    }
    Some(out)
}

/// Parse one `tasklist /FO CSV /NH` line: `"name.exe","1234","Console",...`.
pub fn parse_tasklist_line(line: &str) -> Option<(u32, String)> {
    let mut fields = line.trim().trim_matches('"').split("\",\"");
    let name = fields.next()?.to_string();
    let pid = fields.next()?.parse::<u32>().ok()?;
    Some((pid, name))
}

/// `tasklist` with a hard 3 s bound (killed on timeout). None = could not tell.
#[cfg(windows)]
fn list_processes() -> Option<Vec<(u32, String)>> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut child = std::process::Command::new("tasklist")
        .args(["/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        buf
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(25)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };
    let buf = reader.join().ok()?;
    if !status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).lines().filter_map(parse_tasklist_line).collect())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn list_processes() -> Option<Vec<(u32, String)>> {
    None
}

/// Whether a recognised SDR program (or another azimuth-daemon) runs.
/// `Unknown` (listing failed or timed out) is never read as `NotRunning`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HolderState {
    Running,
    NotRunning,
    Unknown,
}

pub fn holder_state_from(procs: Option<&[(u32, String)]>, own_pid: u32) -> HolderState {
    match procs {
        None => HolderState::Unknown,
        Some(p) if holders_from_processes(p, own_pid).is_empty() => HolderState::NotRunning,
        Some(_) => HolderState::Running,
    }
}

/// Process enumeration only: it never touches the dongle.
pub fn holder_state() -> HolderState {
    holder_state_from(list_processes().as_deref(), std::process::id())
}

/// Likely holders of the SDR right now: recognised running programs, or the
/// generic list when none is recognisable.
pub fn likely_holders() -> Vec<String> {
    let found = holders_from_processes(&list_processes().unwrap_or_default(), std::process::id());
    if found.is_empty() {
        LIKELY_HOLDERS.iter().map(|s| s.to_string()).collect()
    } else {
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, name: &str) -> (u32, String) {
        (pid, name.to_string())
    }

    #[test]
    fn recognises_common_sdr_programs() {
        let procs = [p(10, "SDRSharp.exe"), p(11, "SDR++.exe"), p(12, "explorer.exe"), p(13, "rtl_tcp.exe")];
        let h = holders_from_processes(&procs, 1);
        assert_eq!(h, vec!["SDR#/SDRSharp", "SDR++", "rtl_tcp"]);
    }

    #[test]
    fn another_azimuth_daemon_counts_but_not_this_process() {
        let procs = [p(100, "azimuth-daemon.exe"), p(200, "azimuth-daemon.exe")];
        assert_eq!(holders_from_processes(&procs, 100), vec!["another azimuth-daemon"]);
        assert!(holders_from_processes(&procs[..1], 100).is_empty());
    }

    #[test]
    fn unrelated_processes_match_nothing() {
        assert!(holders_from_processes(&[p(1, "chrome.exe"), p(2, "bash")], 9).is_empty());
    }

    #[test]
    fn tasklist_csv_line_parses() {
        assert_eq!(
            parse_tasklist_line("\"SDRSharp.exe\",\"4242\",\"Console\",\"1\",\"52,100 K\""),
            Some((4242, "SDRSharp.exe".to_string()))
        );
        assert_eq!(parse_tasklist_line("garbage"), None);
    }

    #[test]
    fn likely_holders_never_returns_empty() {
        assert!(!likely_holders().is_empty());
    }

    #[test]
    fn a_failed_listing_is_unknown_never_not_running() {
        assert_eq!(holder_state_from(None, 1), HolderState::Unknown);
        assert_eq!(holder_state_from(Some(&[p(1, "bash")]), 9), HolderState::NotRunning);
        assert_eq!(holder_state_from(Some(&[p(2, "SDRSharp.exe")]), 9), HolderState::Running);
    }
}
