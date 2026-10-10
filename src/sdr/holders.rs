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

/// Run `cmd` (no shell) with a hard 3 s bound over the WHOLE call (child exit AND
/// reading its output): stdout on success, None when it cannot start, exits
/// non-zero, has to be killed, or something else keeps the pipe open past the
/// deadline (the reader thread is then left detached).
#[cfg(any(windows, all(unix, not(any(target_os = "linux", target_os = "android"))), test))]
fn run_bounded(cmd: &mut std::process::Command) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out.read_to_end(&mut buf);
        let _ = tx.send(buf);
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
    let left = deadline.saturating_duration_since(std::time::Instant::now()).max(std::time::Duration::from_millis(250));
    let buf = rx.recv_timeout(left).ok()?;
    status.success().then_some(buf)
}

#[cfg(windows)]
fn list_processes() -> Option<Vec<(u32, String)>> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let buf = run_bounded(
        std::process::Command::new("tasklist")
            .args(["/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW),
    )?;
    Some(String::from_utf8_lossy(&buf).lines().filter_map(parse_tasklist_line).collect())
}

/// Parse one `ps -axo pid=,comm=` line: `  123 /usr/bin/name` (comm may hold spaces
/// and a path; the program name is the last path component).
#[cfg(any(all(unix, not(any(target_os = "linux", target_os = "android"))), test))]
pub fn parse_ps_line(line: &str) -> Option<(u32, String)> {
    let line = line.trim_start();
    let (pid, rest) = line.split_once(char::is_whitespace)?;
    let pid = pid.parse::<u32>().ok()?;
    let name = rest.trim().rsplit('/').next()?.trim();
    (!name.is_empty()).then(|| (pid, name.to_string()))
}

/// macOS and other unix without /proc.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn list_processes() -> Option<Vec<(u32, String)>> {
    let buf = run_bounded(std::process::Command::new("ps").args(["-axo", "pid=,comm="]))?;
    Some(String::from_utf8_lossy(&buf).lines().filter_map(parse_ps_line).collect())
}

/// Android: USB access is an exclusive UsbManager fd, so there is no process-level
/// holder to detect (and toybox `ps` may be sandboxed): report none, recovery unchanged.
#[cfg(target_os = "android")]
fn list_processes() -> Option<Vec<(u32, String)>> {
    Some(Vec::new())
}

#[cfg(not(any(unix, windows)))]
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

/// True when at least one recognised holder runs and EVERY one is another
/// azimuth-daemon (no third-party SDR program).
pub fn only_peer_daemons(procs: &[(u32, String)], own_pid: u32) -> bool {
    let h = holders_from_processes(procs, own_pid);
    !h.is_empty() && h.iter().all(|l| l == "another azimuth-daemon")
}

/// One process listing -> (holder state, holders are peer daemons only).
pub fn holder_snapshot() -> (HolderState, bool) {
    let procs = list_processes();
    let own = std::process::id();
    let peers = procs.as_deref().is_some_and(|p| only_peer_daemons(p, own));
    (holder_state_from(procs.as_deref(), own), peers)
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

    #[cfg(unix)]
    #[test]
    fn run_bounded_never_blocks_when_another_process_keeps_the_pipe_open() {
        let t = std::time::Instant::now();
        // The shell exits at once; the background sleep inherits stdout and keeps the pipe open.
        let r = run_bounded(std::process::Command::new("sh").args(["-c", "(sleep 5) & exit 0"]));
        let took = t.elapsed();
        eprintln!("run_bounded returned {r:?} after {took:?}");
        assert!(r.is_none());
        assert!(took < std::time::Duration::from_millis(3600), "bounded: {took:?}");
        // Control: a normal command still returns its output.
        assert_eq!(run_bounded(std::process::Command::new("sh").args(["-c", "echo hi"])).as_deref(), Some(&b"hi\n"[..]));
    }

    #[test]
    fn only_peer_daemons_is_false_with_a_third_party_or_nobody() {
        let d = p(200, "azimuth-daemon.exe");
        assert!(only_peer_daemons(&[p(100, "azimuth-daemon.exe"), d.clone()], 100));
        assert!(!only_peer_daemons(&[p(100, "azimuth-daemon.exe"), d.clone(), p(5, "SDRSharp.exe")], 100));
        assert!(!only_peer_daemons(&[p(100, "azimuth-daemon.exe"), p(6, "explorer.exe")], 100), "nobody else: control");
        assert!(!only_peer_daemons(&[p(5, "rtl_tcp.exe")], 100));
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

    #[test]
    fn ps_output_parses_pids_paths_and_names_with_spaces() {
        assert_eq!(parse_ps_line("  412 /Applications/SDR++.app/Contents/MacOS/sdrpp"), Some((412, "sdrpp".into())));
        assert_eq!(parse_ps_line("1 /sbin/launchd"), Some((1, "launchd".into())));
        assert_eq!(parse_ps_line(" 90 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome"), Some((90, "Google Chrome".into())));
        assert_eq!(parse_ps_line("77 rtl_tcp"), Some((77, "rtl_tcp".into())));
        assert_eq!(parse_ps_line("PID COMM"), None);
        assert_eq!(parse_ps_line(""), None);
        assert_eq!(parse_ps_line("12"), None);
    }
}
