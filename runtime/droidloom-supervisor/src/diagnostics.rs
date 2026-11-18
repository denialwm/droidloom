//! Read-only, bounded Android diagnostics. No caller-selected executable or shell.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::{Command, Stdio};
use std::thread;

use super::{ControlError, io_error};

const MAX_OUTPUT: usize = 128 * 1024;

pub(super) fn collect(
    pid: u32,
    package: Option<&str>,
    user: u32,
    lines: u32,
    crashes: bool,
) -> Result<String, ControlError> {
    collect_with(package, user, lines, crashes, |args| {
        let mut command = droidloom_cpu_placement::command("/usr/bin/timeout");
        command
            .args(["--kill-after=1s", "10s", "/usr/bin/nsenter", "--target"])
            .arg(pid.to_string())
            .args([
                "--mount", "--uts", "--ipc", "--net", "--pid", "--cgroup", "--root", "--wd",
                "--env", "--",
            ])
            .args(args);
        capture(&mut command)
    })
}

fn collect_with(
    package: Option<&str>,
    user: u32,
    lines: u32,
    crashes: bool,
    mut run: impl FnMut(&[String]) -> Result<String, ControlError>,
) -> Result<String, ControlError> {
    let mut args: Vec<String> = [
        "/system/bin/logcat",
        "-d",
        "-b",
        if crashes {
            "crash"
        } else {
            "main,system,crash"
        },
        "-v",
        "threadtime",
        "-v",
        "printable",
        "-t",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(lines.to_string());
    if let Some(package) = package {
        let listing = run(&[
            "/system/bin/cmd".into(),
            "package".into(),
            "list".into(),
            "packages".into(),
            "-U".into(),
            "--user".into(),
            user.to_string(),
            package.into(),
        ]);
        if crashes {
            // A package Android does not know has no crashes to show. When
            // the package service itself is down, the crash buffer still
            // holds what explains it.
            if let Ok(listing) = listing {
                package_uid(&listing, package)?;
            }
        } else {
            let uid = package_uid(&listing?, package)?;
            args.push(format!("--uid={uid}"));
        }
    }
    // Explicit filter avoids inheriting Android's ambient log tag filters.
    args.push("*:V".into());
    let logs = run(&args);
    if !crashes {
        return logs.map(|text| {
            if text.trim().is_empty() {
                "No matching log entries in the retained Android buffers.\n".into()
            } else {
                text
            }
        });
    }

    let mut exit_args = vec![
        "/system/bin/dumpsys".into(),
        "-t".into(),
        "5".into(),
        "activity".into(),
        "exit-info".into(),
    ];
    if let Some(package) = package {
        exit_args.push(package.into());
    }
    let exits = run(&exit_args);
    if let (Err(log_error), Err(exit_error)) = (&logs, &exits) {
        return Err(ControlError::Invalid(format!(
            "crash buffer unavailable: {log_error}; exit history unavailable: {exit_error}"
        )));
    }
    // Keep global crash traces: native crash reporters and system_server may
    // write under different UIDs, after the app process has already exited.
    let section = |result: Result<String, ControlError>| match result {
        Ok(text) if text.trim().is_empty() => "No retained entries.\n".into(),
        Ok(text) => text,
        Err(error) => format!("Unavailable: {error}\n"),
    };
    Ok(format!(
        "=== Recent crash buffer (all apps, including system crash reporters) ===\n{}\n=== Recorded process exits ({}) ===\n{}",
        section(logs),
        package.unwrap_or("all apps"),
        section(exits)
    ))
}

fn package_uid(listing: &str, package: &str) -> Result<u32, ControlError> {
    let expected = format!("package:{package}");
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        if fields.next() == Some(expected.as_str()) {
            return fields
                .find_map(|field| field.strip_prefix("uid:"))
                .and_then(|uid| uid.parse().ok())
                .ok_or_else(|| {
                    ControlError::Invalid("Android returned an invalid package UID".into())
                });
        }
    }
    Err(ControlError::Invalid(format!(
        "package {package} is not installed for the selected Android user"
    )))
}

fn read_tail(mut reader: impl Read, limit: usize) -> io::Result<String> {
    let mut tail = VecDeque::with_capacity(limit);
    let mut buffer = [0; 8192];
    let mut truncated = false;
    loop {
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        for byte in &buffer[..count] {
            if tail.len() == limit {
                tail.pop_front();
                truncated = true;
            }
            tail.push_back(*byte);
        }
    }
    let bytes: Vec<u8> = tail.into_iter().collect();
    let text = String::from_utf8_lossy(&bytes);
    Ok(if truncated {
        format!("[Older output omitted: showing the last {limit} bytes]\n{text}")
    } else {
        text.into_owned()
    })
}

fn capture(command: &mut Command) -> Result<String, ControlError> {
    capture_with_stdin(command, Stdio::null())
}

pub(super) fn capture_with_stdin(
    command: &mut Command,
    stdin: Stdio,
) -> Result<String, ControlError> {
    let mut child = command
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| io_error("start Android command", source))?;
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (status, stdout, stderr) = thread::scope(|scope| {
        let out = scope.spawn(|| read_tail(stdout, MAX_OUTPUT));
        let err = scope.spawn(|| read_tail(stderr, 16 * 1024));
        (child.wait(), out.join(), err.join())
    });
    let status = status.map_err(|source| io_error("wait for Android command", source))?;
    let read = |result: thread::Result<io::Result<String>>| {
        result
            .map_err(|_| ControlError::Invalid("Android command output reader failed".into()))?
            .map_err(|source| io_error("read Android command output", source))
    };
    let stdout = read(stdout)?;
    let stderr = read(stderr)?;
    if !status.success() {
        if matches!(status.code(), Some(124 | 137)) {
            return Err(super::android_operation_timeout("Android command"));
        }
        return Err(ControlError::Invalid(format!(
            "Android command failed ({status}): {stderr}{stdout}"
        )));
    }
    Ok(if stderr.is_empty() {
        stdout
    } else {
        format!("{stdout}\n{stderr}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logs_use_installed_uid_even_when_the_process_is_dead() {
        let mut calls = Vec::new();
        let result = collect_with(Some("com.example.app"), 10, 200, false, |args| {
            calls.push(args.to_vec());
            Ok(if calls.len() == 1 {
                "package:com.example.app.extra uid:1010002\npackage:com.example.app uid:1010001\n"
                    .into()
            } else {
                "retained stack trace\n".into()
            })
        })
        .unwrap();
        assert_eq!(result, "retained stack trace\n");
        assert!(calls[0].windows(2).any(|args| args == ["--user", "10"]));
        assert!(calls[1].contains(&"--uid=1010001".into()));
        assert!(!calls[1].iter().any(|arg| arg.starts_with("--pid")));
        assert!(package_uid("package:com.example.app.extra uid:10001", "com.example.app").is_err());
        assert!(package_uid("package:com.example.app uid:oops", "com.example.app").is_err());
    }

    #[test]
    fn crash_report_keeps_native_traces_when_activity_service_is_down() {
        let report = collect_with(Some("com.example.app"), 0, 200, true, |args| {
            assert!(!args.iter().any(|arg| arg.starts_with("--uid")));
            if args[0] == "/system/bin/cmd" {
                Err(ControlError::Invalid("package service unavailable".into()))
            } else if args[0] == "/system/bin/logcat" {
                Ok("DEBUG: native backtrace from crash_dump64\n".into())
            } else {
                assert_eq!(args.last().unwrap(), "com.example.app");
                Err(ControlError::Invalid("activity service unavailable".into()))
            }
        })
        .unwrap();
        assert!(report.contains("native backtrace"));
        assert!(report.contains("Unavailable: activity service unavailable"));
    }

    #[test]
    fn crash_report_refuses_a_package_android_does_not_know() {
        let mut calls = 0;
        let error = collect_with(Some("com.android.nonexistent.pkg"), 0, 200, true, |args| {
            calls += 1;
            assert_eq!(args[0], "/system/bin/cmd");
            Ok(String::new())
        })
        .unwrap_err()
        .to_string();
        assert!(error.contains("com.android.nonexistent.pkg is not installed"));
        assert_eq!(calls, 1);
    }

    #[test]
    fn oversized_output_keeps_bounded_tail_and_announces_truncation() {
        let output = read_tail(&b"old records\nlatest trace\n"[..], 13).unwrap();
        assert!(output.starts_with("[Older output omitted:"));
        assert!(output.ends_with("latest trace\n"));
        assert!(!output.contains("old records"));
    }

    #[test]
    fn timed_out_commands_report_failure() {
        let mut command = droidloom_cpu_placement::command("/usr/bin/timeout");
        command.args(["--kill-after=0.1s", "0.1s", "/usr/bin/sleep", "5"]);
        let started = std::time::Instant::now();
        let error = capture(&mut command).unwrap_err().to_string();
        assert!(error.contains("Android looks stuck"));
        assert!(error.contains("journalctl -b -u droidloomd.service"));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
    }
}
