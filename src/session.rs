//! Fail-closed eligibility for COSMIC graphical test accounts.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    process::{Command, Output, Stdio},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub user: String,
    pub uid: u32,
    pub session: String,
    pub seat: String,
    pub runtime_dir: PathBuf,
    pub wayland_display: String,
    pub display: Option<String>,
}

impl SessionInfo {
    pub fn configure(&self, command: &mut Command) {
        command
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .env(
                "DBUS_SESSION_BUS_ADDRESS",
                format!("unix:path={}/bus", self.runtime_dir.display()),
            )
            .env("WAYLAND_DISPLAY", &self.wayland_display)
            .env("XDG_SESSION_ID", &self.session)
            .env("XDG_SESSION_TYPE", "wayland")
            .env("XDG_CURRENT_DESKTOP", "COSMIC")
            .env_remove("WAYLAND_SOCKET");
        if let Some(display) = &self.display {
            command.env("DISPLAY", display);
        } else {
            command.env_remove("DISPLAY");
        }
    }
}

// Every external observation has a deadline; an absent bus must not hang a run.
fn run(program: &str, args: &[&str], runtime: Option<&str>) -> Result<Output> {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=1s", "5s", program])
        .args(args)
        .env("LC_ALL", "C");
    if let Some(runtime) = runtime {
        command.env("XDG_RUNTIME_DIR", runtime).env(
            "DBUS_SESSION_BUS_ADDRESS",
            format!("unix:path={runtime}/bus"),
        );
    }
    command
        .output()
        .with_context(|| format!("blocked: cannot observe test session using {program}"))
}

fn text(program: &str, args: &[&str], runtime: Option<&str>) -> Result<String> {
    let output = run(program, args, runtime)?;
    ensure!(
        output.status.success(),
        "blocked: {program} session observation failed: {}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn properties(text: &str) -> BTreeMap<&str, &str> {
    text.lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

fn validate_session(text: &str, user: &str, uid: u32) -> Result<String> {
    let values = properties(text);
    for (key, expected) in [
        ("User", uid.to_string()),
        ("Name", user.to_owned()),
        ("Type", "wayland".into()),
        ("Class", "user".into()),
        ("Active", "yes".into()),
        ("State", "active".into()),
        ("Desktop", "COSMIC".into()),
        ("LockedHint", "no".into()),
    ] {
        ensure!(
            values.get(key) == Some(&expected.as_str()),
            "blocked: test user {user}: {key} is {:?}, requires {expected}",
            values.get(key)
        );
    }
    let seat = values
        .get("Seat")
        .filter(|seat| !seat.is_empty())
        .context("blocked: test user's session has no seat")?;
    Ok((*seat).to_owned())
}

fn process_present(name: &str, uid: u32) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    for entry in std::fs::read_dir("/proc")? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().parse::<u32>().is_err() {
            continue;
        }
        let path = entry.path();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.uid() != uid {
            continue;
        }
        let Ok(comm) = std::fs::read_to_string(path.join("comm")) else {
            continue;
        };
        if comm.trim() != name && !comm.trim().starts_with(&format!(".{name}")) {
            continue;
        }
        let exe = match std::fs::read_link(path.join("exe")) {
            Ok(exe) => exe,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(error).context("blocked: cannot identify COSMIC session process");
            }
        };
        if executable_matches(&exe, name) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn executable_matches(path: &std::path::Path, name: &str) -> bool {
    path.file_name()
        .and_then(|file| file.to_str())
        .is_some_and(|file| file == name || file == format!(".{name}-wrapped"))
}

#[cfg(target_os = "linux")]
fn probe(user: &str) -> Result<SessionInfo> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    ensure!(
        !user.is_empty() && !user.starts_with('-'),
        "blocked: invalid test user"
    );
    let uid: u32 = text("id", &["-u", "--", user], None)?.parse()?;
    let current: u32 = text("id", &["-u"], None)?.parse()?;
    ensure!(
        current == uid,
        "blocked: run as test user {user} (UID {uid}); invoking UID is {current}"
    );
    let sessions: Vec<serde_json::Value> =
        serde_json::from_str(&text("loginctl", &["list-sessions", "--json=short"], None)?)?;
    let graphical = sessions
        .iter()
        .filter(|s| {
            s["uid"].as_u64() == Some(u64::from(uid))
                && s["seat"].as_str().is_some_and(|seat| !seat.is_empty())
        })
        .collect::<Vec<_>>();
    ensure!(
        graphical.len() == 1,
        "blocked: test user {user} needs exactly one seated session; found {}",
        graphical.len()
    );
    let session = graphical[0]["session"]
        .as_str()
        .context("missing session ID")?
        .to_owned();
    let observe = || {
        text(
            "loginctl",
            &[
                "show-session",
                &session,
                "-p",
                "User",
                "-p",
                "Name",
                "-p",
                "Seat",
                "-p",
                "Type",
                "-p",
                "Class",
                "-p",
                "Active",
                "-p",
                "State",
                "-p",
                "Desktop",
                "-p",
                "LockedHint",
            ],
            None,
        )
    };
    let seat = validate_session(&observe()?, user, uid)?;
    // ponytail: COSMIC only. Its greeter executable acts as the locker when
    // run by a desktop user (cosmic-greeter/src/main.rs). Add a lock-state
    // provider before supporting another DE; logind's hint alone is unreliable.
    ensure!(
        process_present("cosmic-session", uid)? && process_present("cosmic-comp", uid)?,
        "blocked: test user's COSMIC session/compositor is unavailable"
    );
    ensure!(
        !process_present("cosmic-greeter", uid)?,
        "blocked: test user's COSMIC lock screen is running"
    );
    let runtime = format!("/run/user/{uid}");
    let metadata = std::fs::metadata(&runtime)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o077 == 0,
        "blocked: test user's runtime directory has incorrect ownership/permissions"
    );
    let environment = text("systemctl", &["--user", "show-environment"], Some(&runtime))?;
    let env = properties(&environment);
    ensure!(
        env.get("XDG_SESSION_TYPE") == Some(&"wayland")
            && env.get("XDG_CURRENT_DESKTOP") == Some(&"COSMIC"),
        "blocked: test user's manager has no COSMIC Wayland environment"
    );
    let display = env
        .get("WAYLAND_DISPLAY")
        .context("blocked: test user's Wayland display is unset")?;
    ensure!(
        std::path::Path::new(display).components().count() == 1
            && !display.starts_with('.')
            && !display.contains('/'),
        "blocked: invalid Wayland socket name"
    );
    let socket = PathBuf::from(&runtime).join(display);
    let metadata = std::fs::symlink_metadata(&socket)?;
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == uid,
        "blocked: Wayland socket does not belong to the test user"
    );
    wayland_roundtrip(&socket)?;
    // Recheck after I/O: user switching/locking during observation is not ready.
    validate_session(&observe()?, user, uid)?;
    ensure!(
        !process_present("cosmic-greeter", uid)?,
        "blocked: test session locked during readiness check"
    );
    Ok(SessionInfo {
        user: user.into(),
        uid,
        session,
        seat,
        runtime_dir: runtime.into(),
        wayland_display: (*display).into(),
        display: env.get("DISPLAY").map(|s| (*s).into()),
    })
}

#[cfg(target_os = "linux")]
fn wayland_roundtrip(path: &std::path::Path) -> Result<()> {
    use std::{
        io::{Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };
    let mut stream = UnixStream::connect(path)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    // wl_display.sync(new_id wl_callback=2), core wayland.xml, native byte order.
    for word in [1u32, 12u32 << 16, 2u32] {
        stream.write_all(&word.to_ne_bytes())?;
    }
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    ensure!(
        u32::from_ne_bytes(header[..4].try_into()?) == 2
            && u32::from_ne_bytes(header[4..].try_into()?) == 12u32 << 16,
        "blocked: compositor did not acknowledge wl_display.sync"
    );
    let mut serial = [0; 4];
    stream.read_exact(&mut serial)?;
    Ok(())
}

pub fn check(user: &str) -> Result<SessionInfo> {
    #[cfg(target_os = "linux")]
    {
        probe(user).with_context(|| format!("blocked: test session for {user} is not ready"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = user;
        anyhow::bail!("blocked: session readiness currently supports Linux/COSMIC")
    }
}

pub fn exec(user: &str, args: &[String]) -> Result<()> {
    let session = check(user)?;
    let (program, args) = args
        .split_first()
        .context("session-exec requires a command")?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    session.configure(&mut command);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        Err(command.exec()).context("failed to execute session command")
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!("blocked: session-exec requires Unix")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const READY: &str = "User=2001\nName=jarvis\nSeat=seat0\nType=wayland\nClass=user\nActive=yes\nState=active\nDesktop=COSMIC\nLockedHint=no\n";

    #[test]
    fn nix_wrapped_lockers_cannot_hide_behind_truncated_process_names() {
        assert!(executable_matches(
            std::path::Path::new("/nix/store/hash/bin/.cosmic-greeter-wrapped"),
            "cosmic-greeter"
        ));
        assert!(!executable_matches(
            std::path::Path::new("/bin/cosmic-greeter-helper"),
            "cosmic-greeter"
        ));
    }

    #[test]
    fn locked_inactive_wrong_user_and_unknown_sessions_are_blocked() {
        validate_session(READY, "jarvis", 2001).unwrap();
        for (from, to) in [
            ("LockedHint=no", "LockedHint=yes"),
            ("Active=yes", "Active=no"),
            ("State=active", "State=online"),
            ("User=2001", "User=1000"),
            ("Seat=seat0", "Seat="),
            ("Desktop=COSMIC", "Desktop=unknown"),
            ("LockedHint=no", "LockedHint="),
        ] {
            assert!(
                validate_session(&READY.replace(from, to), "jarvis", 2001).is_err(),
                "{to}"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn compositor_roundtrip_requires_a_real_sync_acknowledgement() {
        use std::{
            io::{Read, Write},
            os::unix::net::UnixListener,
        };
        for valid in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let socket = dir.path().join("wayland-1");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 12];
                stream.read_exact(&mut request).unwrap();
                assert_eq!(u32::from_ne_bytes(request[..4].try_into().unwrap()), 1);
                for word in [if valid { 2u32 } else { 1u32 }, 12u32 << 16, 0u32] {
                    stream.write_all(&word.to_ne_bytes()).unwrap();
                }
            });
            assert_eq!(wayland_roundtrip(&socket).is_ok(), valid);
            server.join().unwrap();
        }
    }
}
