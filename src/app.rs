//! Per-product attribution (macOS). The privacy prompts macOS shows when an
//! agent opens ~/Downloads, records the screen, … name the agent's
//! "responsible process": by default the agl daemon, so every product that
//! drives agents through agl shows up as "agl". A session created with an
//! `app_name` runs its agent under a small launcher inside
//! `$AGL_HOME/apps/<app_name>.app` that disclaims responsibility (as
//! Terminal and VS Code do for their child processes), so the prompt names
//! that product instead, and each product keeps its own grants.
//!
//! The launcher is a copy of agl run as `<launcher> __hold <program> <args>`:
//! it re-executes itself in place as a process responsible for itself, starts
//! the program and waits for it, forwarding signals. The copy is only
//! replaced when the launcher protocol changes (`HOLDER_REV`), so updating
//! agl keeps the code identity macOS stored the grants under.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// First argument that runs agl as a product launcher.
pub const HOLD_ARG: &str = "__hold";
/// The launcher after it took responsibility for itself.
const HELD_ARG: &str = "__held";
/// Bump when the launcher's command line or behavior changes: launchers are
/// rebuilt then (and their privacy grants asked for again).
const HOLDER_REV: &str = "1";
pub const MAX_NAME_CHARS: usize = 48;

/// A usable `app_name`: trimmed, at most 48 characters, no control
/// characters, `/` or `:`, not starting with `.`. `None` for an empty name or
/// "agl" itself (no launcher needed).
pub fn validate(name: &str) -> Result<Option<String>> {
    let name = name.trim();
    if name.is_empty() || name.eq_ignore_ascii_case("agl") {
        return Ok(None);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        bail!("`app_name` is at most {MAX_NAME_CHARS} characters");
    }
    if name.starts_with('.') || name.chars().any(|c| c.is_control() || c == '/' || c == ':') {
        bail!("`app_name` cannot start with `.` or contain `/`, `:` or control characters");
    }
    Ok(Some(name.to_string()))
}

/// The launcher executable for `name`, built on first use. `None` where
/// macOS attribution does not apply (other systems) or the bundle could not
/// be built (the agent then runs as before, attributed to agl).
pub fn launcher(name: &str) -> Option<PathBuf> {
    // The launcher is a copy of this executable: only one that handles
    // `__hold` (it called `hold_from_args` first thing) can serve.
    if !cfg!(target_os = "macos") || !CAN_HOLD.load(std::sync::atomic::Ordering::SeqCst) {
        return None;
    }
    match ensure_bundle(&crate::config::home().join("apps"), name) {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("app `{name}`: no launcher ({e:#}); the agent runs as agl");
            None
        }
    }
}

/// The program to start for `bin` on behalf of `app_name`, and the
/// arguments that go before the program's own.
pub fn wrap(bin: &Path, app_name: Option<&str>) -> (PathBuf, Vec<std::ffi::OsString>) {
    match app_name.and_then(launcher) {
        Some(l) => (l, vec![HOLD_ARG.into(), bin.as_os_str().to_owned()]),
        None => (bin.to_path_buf(), Vec::new()),
    }
}

fn bundle_id(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let slug: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-");
    if !slug.is_empty() && name.is_ascii() {
        return format!("cc.agl.app.{slug}");
    }
    let hash: String = Sha256::digest(name.as_bytes()).iter().take(6).map(|b| format!("{b:02x}")).collect();
    if slug.is_empty() {
        format!("cc.agl.app.x{hash}")
    } else {
        format!("cc.agl.app.{slug}-{hash}")
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn info_plist(name: &str, id: &str, exe: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>{exe}</string>
  <key>CFBundleIdentifier</key><string>{id}</string>
  <key>CFBundleName</key><string>{name}</string>
  <key>CFBundleDisplayName</key><string>{name}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>{rev}</string>
  <key>CFBundleVersion</key><string>{rev}</string>
  <key>LSUIElement</key><true/>
</dict>
</plist>
"#,
        exe = xml(exe),
        id = xml(id),
        name = xml(name),
        rev = HOLDER_REV,
    )
}

static BUILD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// `<root>/<name>.app` with a copy of this agl as its executable, ad-hoc
/// signed under a stable identifier. Reused while its `HOLDER_REV` matches.
fn ensure_bundle(root: &Path, name: &str) -> Result<PathBuf> {
    let _guard = BUILD.lock().unwrap_or_else(|e| e.into_inner());
    let dir = root.join(format!("{name}.app"));
    let exe = dir.join("Contents/MacOS").join(name);
    let marker = dir.join("Contents/Resources/agl-launcher");
    if exe.is_file() && std::fs::read_to_string(&marker).is_ok_and(|r| r.trim() == HOLDER_REV) {
        return Ok(exe);
    }
    let me = std::env::current_exe().context("locate agl")?;
    let tmp = root.join(format!(".build-{}", uuid::Uuid::new_v4().simple()));
    let built = (|| {
        std::fs::create_dir_all(tmp.join("Contents/MacOS"))?;
        std::fs::create_dir_all(tmp.join("Contents/Resources"))?;
        let id = bundle_id(name);
        std::fs::copy(&me, tmp.join("Contents/MacOS").join(name)).context("copy agl")?;
        std::fs::write(tmp.join("Contents/Info.plist"), info_plist(name, &id, name))?;
        std::fs::write(tmp.join("Contents/Resources/agl-launcher"), HOLDER_REV)?;
        // Seal the Info.plist into the signature; macOS names the bundle
        // without it too, so a missing codesign is not fatal.
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--identifier", &id])
            .arg(&tmp)
            .output();
        if let Ok(out) = &signed {
            if !out.status.success() {
                eprintln!("app `{name}`: codesign failed: {}", String::from_utf8_lossy(&out.stderr).trim());
            }
        }
        anyhow::Ok(())
    })();
    if let Err(e) = built {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    // A launcher still running from the old bundle keeps its files open.
    if dir.exists() {
        let old = root.join(format!(".old-{}", uuid::Uuid::new_v4().simple()));
        std::fs::rename(&dir, &old)?;
        let _ = std::fs::remove_dir_all(&old);
    }
    std::fs::rename(&tmp, &dir).with_context(|| format!("create {}", dir.display()))?;
    Ok(exe)
}

static CAN_HOLD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Run as a launcher when agl was started as one (`__hold` / `__held`):
/// returns the exit code to exit with, or `None` for an ordinary agl run.
/// Call it before anything else in `main`; an app embedding agl's session
/// manager does the same to give its sessions' `app_name` effect.
pub fn hold_from_args() -> Option<i32> {
    CAN_HOLD.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut args = std::env::args_os();
    let me = args.next()?;
    let mode = args.next()?;
    let rest: Vec<std::ffi::OsString> = args.collect();
    if mode == HOLD_ARG {
        if rest.is_empty() {
            eprintln!("agl {HOLD_ARG}: no program");
            return Some(2);
        }
        #[cfg(target_os = "macos")]
        disclaim_and_reexec(&me, &rest);
        let _ = me;
        return Some(hold(&rest));
    }
    if mode == HELD_ARG {
        return Some(hold(&rest));
    }
    None
}

/// Replace this process with `<self> __held <rest>`, responsible for itself.
/// Returns only if that fails; the launcher then goes on attributed to its
/// parent, as an agent without `app_name` would be.
#[cfg(target_os = "macos")]
fn disclaim_and_reexec(me: &std::ffi::OsStr, rest: &[std::ffi::OsString]) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    extern "C" {
        fn responsibility_spawnattrs_setdisclaim(attrs: *mut libc::posix_spawnattr_t, disclaim: libc::c_int) -> libc::c_int;
    }
    // The path of the running executable: argv[0] may be relative.
    let path = std::env::current_exe().map(|p| p.into_os_string()).unwrap_or_else(|_| me.to_owned());
    let Ok(cpath) = CString::new(path.as_bytes()) else { return };
    let mut argv: Vec<CString> = Vec::with_capacity(rest.len() + 2);
    for a in std::iter::once(me).chain(std::iter::once(std::ffi::OsStr::new(HELD_ARG))).chain(rest.iter().map(|a| a.as_os_str())) {
        let Ok(c) = CString::new(a.as_bytes()) else { return };
        argv.push(c);
    }
    let mut ptrs: Vec<*mut libc::c_char> = argv.iter().map(|c| c.as_ptr() as *mut libc::c_char).collect();
    ptrs.push(std::ptr::null_mut());
    extern "C" {
        static environ: *const *mut libc::c_char;
    }
    // SAFETY: plain posix_spawn(3) with POSIX_SPAWN_SETEXEC on NUL-terminated
    // argv/env arrays that outlive the call; on success it does not return.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
        if libc::posix_spawnattr_init(&mut attr) != 0 {
            return;
        }
        let ok = responsibility_spawnattrs_setdisclaim(&mut attr, 1) == 0
            && libc::posix_spawnattr_setflags(&mut attr, libc::POSIX_SPAWN_SETEXEC as libc::c_short) == 0;
        if ok {
            let mut pid: libc::pid_t = 0;
            let rc = libc::posix_spawn(&mut pid, cpath.as_ptr(), std::ptr::null(), &attr, ptrs.as_ptr(), environ);
            eprintln!("agl {HOLD_ARG}: could not take responsibility (posix_spawn: {rc}); running as is");
        }
        libc::posix_spawnattr_destroy(&mut attr);
    }
}

static CHILD: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

#[cfg(unix)]
extern "C" fn forward(sig: libc::c_int) {
    let pid = CHILD.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        // SAFETY: kill(2) is async-signal-safe.
        unsafe { libc::kill(pid, sig) };
    }
}

/// Start the program with this process's stdio, environment and directory,
/// pass signals on, and exit as it did.
fn hold(rest: &[std::ffi::OsString]) -> i32 {
    let mut child = match std::process::Command::new(&rest[0]).args(&rest[1..]).spawn() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("agl: cannot start {}: {e}", rest[0].to_string_lossy());
            return 127;
        }
    };
    #[cfg(unix)]
    {
        CHILD.store(child.id() as i32, std::sync::atomic::Ordering::SeqCst);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT, libc::SIGUSR1, libc::SIGUSR2] {
            // SAFETY: installs a handler that only calls kill(2).
            unsafe { libc::signal(sig, forward as *const () as libc::sighandler_t) };
        }
    }
    let status = loop {
        match child.wait() {
            Ok(s) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return 1,
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(sig) = status.signal() {
            // Die the same way, so the daemon sees what happened to the agent.
            unsafe {
                libc::signal(sig, libc::SIG_DFL);
                libc::kill(libc::getpid(), sig);
            }
            return 128 + sig;
        }
    }
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(validate("  Sumus ").unwrap().as_deref(), Some("Sumus"));
        assert_eq!(validate("豆聊 Douchat").unwrap().as_deref(), Some("豆聊 Douchat"));
        assert_eq!(validate("").unwrap(), None);
        assert_eq!(validate("AGL").unwrap(), None);
        assert!(validate("a/b").is_err());
        assert!(validate(".hidden").is_err());
        assert!(validate("x\ny").is_err());
        assert!(validate(&"x".repeat(49)).is_err());
        assert_eq!(bundle_id("Sumus"), "cc.agl.app.sumus");
        assert_eq!(bundle_id("Dou Chat!"), "cc.agl.app.dou-chat");
        assert!(bundle_id("豆聊").starts_with("cc.agl.app.x"));
        assert_ne!(bundle_id("豆聊"), bundle_id("豆包"));
        assert!(info_plist("A&B", "cc.agl.app.a-b", "A&B").contains("<string>A&amp;B</string>"));
    }
}
