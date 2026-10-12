//! X3, end to end: the real daemon binary, its status port held by a process
//! that is not an Azimuth daemon. The in-crate tests only check the code that
//! `bind_with_policy` returns; this proves the process really exits with 42 and
//! says why, instead of running headless.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn port_held_by_a_foreign_process_exits_42_with_an_explicit_error() {
    // A free port, then held by a dummy that accepts and drops every connection
    // (so the daemon's sibling probe fails fast and classifies it as foreign).
    let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("bind dummy holder");
    let addr = holder.local_addr().unwrap().to_string();
    std::thread::spawn(move || {
        for conn in holder.incoming() {
            drop(conn);
        }
    });

    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("x3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("config.toml");
    let example = include_str!("../config.example.toml");
    assert!(example.contains("status_bind = \"127.0.0.1:8420\""), "example config no longer names the status port");
    std::fs::write(&config, example.replace("127.0.0.1:8420", &addr)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_azimuth-daemon"))
        .arg("--config")
        .arg(&config)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start the daemon binary");

    // The bind policy retries for about 26 s before giving up.
    let deadline = Instant::now() + Duration::from_secs(120);
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("daemon kept running with its status port held by another process");
        }
        std::thread::sleep(Duration::from_millis(200));
    };

    let mut output = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut output);
    }
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut output);
    }
    assert_eq!(status.code(), Some(42), "exit code; output:\n{output}");
    assert!(
        output.contains("Port is held by a non-Azimuth process") && output.contains("exiting 42"),
        "explicit error line missing; output:\n{output}"
    );
    assert!(output.contains(&addr), "the error must name the address {addr}; output:\n{output}");
}
