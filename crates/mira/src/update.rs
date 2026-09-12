//! `mira update` — replace this binary with a release from GitHub.
//!
//! It does that by re-running the installer, not by re-implementing it.
//! `scripts/get-mira.sh` already resolves the latest tag, picks the Rust target
//! triple, verifies `SHA256SUMS`, verifies the build attestation when `gh` is
//! present, and elevates only for the final move. Doing any of that from Rust
//! needs an HTTPS client, and the smallest one worth trusting costs `rustls`
//! plus `ring` plus their trees — against a dependency count the README
//! publishes as a product property. Shelling out keeps that number where it is
//! and keeps the installer singular: the same bytes that installed mira update
//! it, so there is no second code path to be wrong by the next release.
//!
//! There is no `--check`. The installer prints `mira vX is already installed`
//! and stops when the version matches, so running it *is* the check, and a
//! second network path that only prints would be a second thing to keep true.
//!
//! The one thing this adds on top of the script is `MIRA_INSTALL_DIR`. The
//! script defaults to `/usr/local/bin`; someone who put mira in `~/.local/bin`
//! and typed `mira update` means *this* mira, not a second copy that then
//! shadows it depending on `PATH` order.

use std::path::{Path, PathBuf};

/// Where the installer is published. `docs/install.sh` is a symlink to
/// `scripts/get-mira.sh`, so this URL and the one in the install docs are the
/// same file.
const INSTALLER: &str = "https://miradb.dev/install.sh";

/// Where to go when there is no shell to run the installer with.
const RELEASES: &str = "https://github.com/TrianaLab/mira/releases";

pub const USAGE: &str = "mira update [--version VERSION] [--dry-run]

Downloads the latest release from GitHub and replaces this binary with it,
by running the same installer as
  curl -fsSL https://miradb.dev/install.sh | bash

  --version VERSION  install this tag instead of the latest (e.g. v0.1.0)
  --dry-run          print the command that would run, and stop

Installs over this binary's own directory, not /usr/local/bin, unless
MIRA_INSTALL_DIR says otherwise. Nothing happens if the running version is
already the one that would be installed.

Needs bash and either curl or wget, because it runs the installer rather
than carrying an HTTPS client. The container image has none of them; upgrade
that by pulling a newer tag.";

/// What `--version` was given, if anything.
///
/// A tag goes into a shell command, so it is checked against a charset rather
/// than quoted: quoting is a thing to get subtly wrong once, and no real Git
/// tag needs a character outside this set. Unknown flags are refused instead of
/// forwarded — the installer's own flag set is not this one's, and silently
/// passing `--no-sudo` through would make its behaviour depend on a flag this
/// usage does not document.
pub fn parse(argv: &[String]) -> Result<(Option<String>, bool), String> {
    let mut version = None;
    let mut dry_run = false;
    let mut it = argv.iter();
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--version" | "-v" => {
                let v = it.next().ok_or("--version needs a value")?;
                if v.is_empty()
                    || !v
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                {
                    return Err(format!("--version: {v:?} is not a release tag"));
                }
                version = Some(v.clone());
            }
            "--dry-run" => dry_run = true,
            other => return Err(format!("unknown flag {other:?}\n\n{USAGE}")),
        }
    }
    Ok((version, dry_run))
}

/// The shell line that does the update.
///
/// `curl` or `wget` is chosen inside the shell rather than out here: the script
/// itself already has to make that choice for every download it does, and one
/// `command -v` in a string is smaller than a probe, an enum and a match.
pub fn command(version: Option<&str>) -> String {
    let tag = match version {
        Some(v) => format!(" --version {v}"),
        None => String::new(),
    };
    format!(
        "if command -v curl >/dev/null 2>&1; then curl -fsSL {INSTALLER}; \
         elif command -v wget >/dev/null 2>&1; then wget -qO- {INSTALLER}; \
         else echo 'mira update needs curl or wget' >&2; exit 1; fi \
         | bash -s --{tag}"
    )
}

/// The directory to install into, given this process's own executable path.
///
/// `None` when it cannot be resolved or has no parent, which leaves
/// `MIRA_INSTALL_DIR` unset and the script on its `/usr/local/bin` default —
/// the right fallback, because a mira that cannot find itself is more likely to
/// be a test harness than a real install.
pub fn install_dir(exe: Option<&Path>) -> Option<PathBuf> {
    let real = std::fs::canonicalize(exe?).ok()?;
    real.parent().map(Path::to_path_buf)
}

/// Run it.
///
/// The child inherits stdio, so the installer's own progress and its `sudo`
/// prompt reach the terminal directly. Its exit status becomes this command's:
/// a failed download must not look like a successful update to whatever ran
/// `mira update` in a script.
pub fn run(argv: &[String]) -> Result<(), String> {
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    let (version, dry_run) = parse(argv)?;
    let line = command(version.as_deref());
    if dry_run {
        println!("{line}");
        return Ok(());
    }

    spawn(&mut installer(&line))
}

/// The child, configured but not started.
///
/// Separate from [`spawn`] so a test can assert what would run — the program,
/// the shell line, the install directory — without a test run downloading a
/// release over the binary that is running the test.
fn installer(line: &str) -> std::process::Command {
    let mut cmd = std::process::Command::new("bash");
    cmd.arg("-c").arg(line);
    if std::env::var_os("MIRA_INSTALL_DIR").is_none() {
        if let Some(dir) = install_dir(std::env::current_exe().ok().as_deref()) {
            cmd.env("MIRA_INSTALL_DIR", dir);
        }
    }
    cmd
}

/// Start it and turn its exit into this command's.
///
/// Taking a `Command` rather than the line means both failure arms — the child
/// that never started and the child that started and failed — are reachable
/// from a test with `true`, `false` and a path that does not exist.
fn spawn(cmd: &mut std::process::Command) -> Result<(), String> {
    let status = cmd
        .status()
        .map_err(|e| start_failed(&cmd.get_program().to_string_lossy(), &e))?;
    if !status.success() {
        return Err(format!("installer exited with {status}"));
    }
    Ok(())
}

/// What to say about a child that never started.
///
/// `NotFound` is not an unusual system here, it is the shipped one: the image
/// is distroless, so it has no bash, no curl and no writable install directory,
/// and the raw "No such file or directory (os error 2)" names the symptom while
/// hiding the answer — a container upgrades by pulling a newer tag, not by
/// rewriting its own rootfs. Every other tool the installer needs already
/// refuses by name (curl-or-wget in [`command`], sha256sum in the script), so
/// this is the last case that did not.
fn start_failed(program: &str, e: &std::io::Error) -> String {
    if e.kind() == std::io::ErrorKind::NotFound {
        return format!(
            "`{program}` is not on PATH, so there is nothing here to run the \
             installer with. In a container, upgrade by pulling a newer image \
             tag. Otherwise install {program}, or take the tarball for this \
             platform straight from {RELEASES}."
        );
    }
    format!("could not run the installer: {e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_flags_installs_the_latest_release() {
        assert_eq!(parse(&[]).unwrap(), (None, false));
        let line = command(None);
        assert!(line.ends_with("| bash -s --"), "{line}");
        assert!(line.contains(INSTALLER), "{line}");
    }

    #[test]
    fn a_tag_is_forwarded_to_the_installer() {
        let (v, dry) = parse(&args(&["--version", "v0.1.0"])).unwrap();
        assert_eq!(v.as_deref(), Some("v0.1.0"));
        assert!(!dry);
        assert!(command(v.as_deref()).ends_with("--version v0.1.0"));
    }

    #[test]
    fn a_tag_that_could_be_a_shell_command_is_refused_rather_than_quoted() {
        for bad in ["v1; rm -rf /", "$(id)", "`id`", "v1 --no-sudo", ""] {
            let e = parse(&args(&["--version", bad])).unwrap_err();
            assert!(e.starts_with("--version:"), "{bad:?} was accepted: {e}");
        }
        assert_eq!(
            parse(&args(&["--version"])).unwrap_err(),
            "--version needs a value"
        );
    }

    #[test]
    fn an_installer_flag_this_command_does_not_document_is_refused() {
        let e = parse(&args(&["--no-sudo"])).unwrap_err();
        assert!(e.starts_with("unknown flag \"--no-sudo\""), "{e}");
        assert!(e.contains(USAGE), "the usage is part of the message");
    }

    #[test]
    fn the_downloader_is_chosen_by_the_shell_and_not_assumed() {
        let line = command(None);
        assert!(line.contains("command -v curl"), "{line}");
        assert!(line.contains("command -v wget"), "{line}");
        assert!(line.contains("needs curl or wget"), "{line}");
    }

    #[test]
    fn the_install_directory_is_this_binarys_own() {
        let exe = std::env::current_exe().unwrap();
        assert_eq!(
            install_dir(Some(&exe)).unwrap(),
            std::fs::canonicalize(&exe).unwrap().parent().unwrap()
        );
        assert_eq!(install_dir(None), None);
        assert_eq!(install_dir(Some(Path::new("/no/such/mira"))), None);
    }

    #[test]
    fn dry_run_prints_the_command_instead_of_running_it() {
        run(&args(&["--dry-run", "--version", "v9.9.9"])).unwrap();
        run(&args(&["--help"])).unwrap();
        assert!(run(&args(&["--nope"])).is_err());
    }

    /// Asserts the child rather than running it: the real one would replace the
    /// binary under test with a download.
    #[test]
    fn the_child_is_the_installer_pointed_at_this_binarys_directory() {
        let line = command(None);
        let cmd = installer(&line);
        assert_eq!(cmd.get_program(), "bash");
        let argv: Vec<_> = cmd.get_args().collect();
        assert_eq!(argv, ["-c", line.as_str()]);
        // The harness runs with `MIRA_INSTALL_DIR` unset, so the override is the
        // one this command adds, and it points at wherever the test binary is.
        let dir = cmd
            .get_envs()
            .find(|(k, _)| *k == "MIRA_INSTALL_DIR")
            .and_then(|(_, v)| v)
            .expect("an install directory");
        let exe = std::env::current_exe().unwrap();
        assert_eq!(Path::new(dir), install_dir(Some(&exe)).unwrap());
    }

    #[test]
    fn a_failed_installer_is_a_failed_update() {
        spawn(&mut std::process::Command::new("true")).unwrap();

        let e = spawn(&mut std::process::Command::new("false")).unwrap_err();
        assert!(e.starts_with("installer exited with"), "{e}");

        let e = spawn(&mut std::process::Command::new("/no/such/installer")).unwrap_err();
        assert!(e.starts_with("`/no/such/installer` is not on PATH"), "{e}");
    }

    /// The distroless image has no bash, and that has to read as an answer
    /// rather than as an errno.
    #[test]
    fn no_shell_says_so_and_says_what_to_do_instead() {
        let e = start_failed("bash", &std::io::ErrorKind::NotFound.into());
        assert!(e.starts_with("`bash` is not on PATH"), "{e}");
        assert!(e.contains("pulling a newer image tag"), "{e}");
        assert!(e.contains(RELEASES), "{e}");

        // Anything else is a real error and is reported as one, rather than
        // being explained away as a missing shell.
        let e = start_failed("bash", &std::io::ErrorKind::PermissionDenied.into());
        assert!(e.starts_with("could not run the installer:"), "{e}");
    }

    #[test]
    fn the_usage_names_what_it_needs_on_the_host() {
        assert!(
            USAGE.contains("Needs bash and either curl or wget"),
            "{USAGE}"
        );
    }
}
