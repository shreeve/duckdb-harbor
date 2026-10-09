//! The copies of harbor on this machine, and which one a bare `harbor` runs.
//!
//! There are two ways to install harbor: install.sh, which puts it in
//! `~/.local/bin`, and Homebrew, which keeps it in its own cellar. Each
//! upgrades itself (`harbor update`, `brew upgrade`) and knows nothing of the
//! other, so a machine with both ends up running whichever comes first in
//! PATH while the other falls behind, and the version a person sees depends
//! on how a thing was launched. That is easy to detect and a mystery to meet,
//! so the list and `update` say so when it is the case, and say how to be rid
//! of the extra one.

use std::path::{Path, PathBuf};

/// How a copy got here, as far as its real path says.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    /// Under a Homebrew cellar: Homebrew put it there and keeps the record.
    Homebrew,
    /// Where the install script puts it (see `script_dir`).
    Script,
    /// Anywhere else: a build, a hand-placed binary, a system-wide install.
    Other,
}

/// One copy of harbor: where it was found, and what that resolves to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Install {
    /// The path as found, the one a person would type or recognize.
    pub path: PathBuf,
    pub channel: Channel,
    /// What makes two finds the same copy: the cellar's formula directory
    /// for a Homebrew copy, whose launcher and binary are two files of one
    /// install, and the resolved file for anything else.
    key: PathBuf,
}

#[cfg(windows)]
const NAME: &str = "harbor.exe";
#[cfg(not(windows))]
const NAME: &str = "harbor";

/// Where the install script puts harbor: install.sh in `~/.local/bin`,
/// install.ps1 in `%LOCALAPPDATA%\Programs\harbor\bin`.
fn script_dir(home: Option<&Path>) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        let _ = home;
        std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join(r"Programs\harbor\bin"))
    }
    #[cfg(not(windows))]
    {
        home.map(|h| h.join(".local/bin"))
    }
}

/// The Homebrew formula a resolved path belongs to (`duckdb-harbor` in
/// `/opt/homebrew/Cellar/duckdb-harbor/0.43.4/libexec/bin/harbor`), and the
/// path up to and including it.
fn cellar(real: &Path) -> Option<(String, PathBuf)> {
    let mut seen = PathBuf::new();
    let mut parts = real.components();
    while let Some(part) = parts.next() {
        seen.push(part);
        if part.as_os_str() == "Cellar" {
            let formula = parts.next()?;
            seen.push(formula);
            return Some((formula.as_os_str().to_string_lossy().into_owned(), seen));
        }
    }
    None
}

/// The formula to name in `brew upgrade`, when `real` is a Homebrew copy.
pub fn formula_of(real: &Path) -> Option<String> {
    cellar(real).map(|(formula, _)| formula)
}

impl Install {
    /// The copy at `path`, or None when nothing runnable is there.
    pub fn at(path: &Path, home: Option<&Path>) -> Option<Install> {
        let real = path.canonicalize().ok()?;
        if !real.is_file() {
            return None;
        }
        let (channel, key) = match cellar(&real) {
            Some((_, root)) => (Channel::Homebrew, root),
            None => {
                let script = script_dir(home)
                    .and_then(|d| d.join(NAME).canonicalize().ok())
                    .is_some_and(|s| s == real);
                (if script { Channel::Script } else { Channel::Other }, real)
            }
        };
        Some(Install { path: path.to_path_buf(), channel, key })
    }

    pub fn same_copy(&self, other: &Install) -> bool {
        self.key == other.key
    }

    /// How to remove this copy, when there is a command for it.
    fn removal(&self) -> Option<String> {
        match self.channel {
            Channel::Homebrew => {
                Some(format!("brew uninstall {}", formula_of(&self.key).unwrap_or_else(|| "duckdb-harbor".into())))
            }
            Channel::Script if cfg!(windows) => Some(
                r#"Remove-Item -Recurse -Force "$env:LOCALAPPDATA\Programs\harbor""#.into(),
            ),
            Channel::Script => Some(
                "curl -fsSL https://raw.githubusercontent.com/shreeve/duckdb-harbor/main/install.sh | bash -s -- --uninstall"
                    .into(),
            ),
            Channel::Other => None,
        }
    }
}

/// Every distinct copy among `candidates`, in the order given: the first
/// place a copy is found is the place it is named by.
pub fn distinct(candidates: impl IntoIterator<Item = PathBuf>, home: Option<&Path>) -> Vec<Install> {
    let mut found: Vec<Install> = Vec::new();
    for path in candidates {
        if let Some(install) = Install::at(&path, home)
            && !found.iter().any(|f| f.same_copy(&install))
        {
            found.push(install);
        }
    }
    found
}

/// Where a copy can be: every directory in PATH, in PATH's order, then the
/// places the two installers use, for a copy that is installed but not on
/// this shell's PATH (a GUI app's PATH is shorter than a terminal's).
fn candidates(path_var: Option<&std::ffi::OsStr>, home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_var.map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
    dirs.extend(script_dir(home));
    for prefix in ["/opt/homebrew/bin", "/usr/local/bin", "/home/linuxbrew/.linuxbrew/bin"] {
        dirs.push(PathBuf::from(prefix));
    }
    dirs.into_iter().map(|d| d.join(NAME)).collect()
}

/// What to say about the other copies, given the copy that is running, the
/// copies found (PATH's first), and a way to ask a copy its version. None
/// when this is the only one.
fn describe(this: &Install, found: &[Install], version_of: &dyn Fn(&Path) -> Option<String>) -> Option<String> {
    let others: Vec<&Install> = found.iter().filter(|f| !f.same_copy(this)).collect();
    if others.is_empty() {
        return None;
    }
    let shown = |p: &Path| harbor_common::paths::shorten(p);
    let mut lines = Vec::new();
    for other in &others {
        let mut about = Vec::new();
        if let Some(v) = version_of(&other.path) {
            about.push(v);
        }
        match other.channel {
            Channel::Homebrew => about.push("Homebrew".into()),
            Channel::Script => about.push("install.sh".into()),
            Channel::Other => {}
        }
        let about = if about.is_empty() { String::new() } else { format!(" ({})", about.join(", ")) };
        let mut line = format!("harbor: another copy is installed: {}{about}", shown(&other.path));
        if let Some(removal) = other.removal() {
            line.push_str(&format!("\n        remove it with: {removal}"));
        }
        lines.push(line);
    }
    // What a bare `harbor` runs is the first copy PATH reaches.
    let first = found.first()?;
    lines.push(if first.same_copy(this) {
        format!(
            "harbor: typing `harbor` runs this copy, {}. Two copies upgrade separately and drift apart; keep one.",
            shown(&first.path)
        )
    } else {
        format!(
            "harbor: typing `harbor` runs {}, not this copy ({}). Two copies upgrade separately and drift apart; keep one.",
            shown(&first.path),
            shown(&this.path)
        )
    });
    Some(lines.join("\n"))
}

/// The note for this process, or None when it is the only copy. Asks each
/// other copy its version, so it costs a process per extra copy and nothing
/// when there is none.
pub fn note() -> Option<String> {
    let home = std::env::home_dir();
    let exe = std::env::current_exe().ok()?;
    let this = Install::at(&exe, home.as_deref())?;
    let found = distinct(candidates(std::env::var_os("PATH").as_deref(), home.as_deref()), home.as_deref());
    describe(&this, &found, &|path| {
        let out = std::process::Command::new(path).arg("--version").output().ok()?;
        String::from_utf8(out.stdout).ok()?.trim().strip_prefix("harbor ").map(str::to_string)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// A scratch tree shaped like a machine with both installers' copies:
    /// install.sh's in `home/.local/bin`, and Homebrew's cellar with its
    /// launcher, its binary, the `opt` link and the `bin` link.
    fn machine(tag: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("harbor-installs-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let home = root.join("home");
        let script = home.join(".local/bin");
        fs::create_dir_all(&script).unwrap();
        fs::write(script.join(NAME), "script copy").unwrap();

        let keg = root.join("brew/Cellar/duckdb-harbor/0.43.4");
        fs::create_dir_all(keg.join("bin")).unwrap();
        fs::create_dir_all(keg.join("libexec/bin")).unwrap();
        fs::write(keg.join("bin").join(NAME), "launcher").unwrap();
        fs::write(keg.join("libexec/bin").join(NAME), "brew copy").unwrap();
        let brew_bin = root.join("brew/bin");
        fs::create_dir_all(&brew_bin).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(keg.join("bin").join(NAME), brew_bin.join(NAME)).unwrap();
            fs::create_dir_all(root.join("brew/opt")).unwrap();
            std::os::unix::fs::symlink(&keg, root.join("brew/opt/duckdb-harbor")).unwrap();
        }
        (root, home, script.join(NAME), brew_bin.join(NAME))
    }

    #[cfg(unix)]
    #[test]
    fn a_homebrew_launcher_and_its_binary_are_one_copy() {
        let (root, home, script, brew) = machine("one");
        let through_opt = root.join("brew/opt/duckdb-harbor/libexec/bin").join(NAME);
        let found = distinct([script.clone(), brew.clone(), through_opt.clone(), script.clone()], Some(&home));
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!((found[0].channel, found[1].channel), (Channel::Script, Channel::Homebrew));
        // The launcher in bin and the binary reached through opt are the same install.
        let a = Install::at(&brew, Some(&home)).unwrap();
        let b = Install::at(&through_opt, Some(&home)).unwrap();
        assert!(a.same_copy(&b));
        assert_eq!(formula_of(&through_opt.canonicalize().unwrap()).as_deref(), Some("duckdb-harbor"));
        assert_eq!(formula_of(&script.canonicalize().unwrap()), None);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn the_note_names_the_other_copy_how_to_remove_it_and_which_one_runs() {
        let (root, home, script, brew) = machine("note");
        let version = |p: &Path| Some(if p.starts_with(root.join("brew")) { "0.43.3".to_string() } else { "0.43.4".to_string() });

        // Running the script copy, which PATH reaches first.
        let found = distinct([script.clone(), brew.clone()], Some(&home));
        let this = Install::at(&script, Some(&home)).unwrap();
        let note = describe(&this, &found, &version).unwrap();
        assert!(note.contains("another copy is installed:"), "{note}");
        assert!(note.contains("(0.43.3, Homebrew)"), "{note}");
        assert!(note.contains("remove it with: brew uninstall duckdb-harbor"), "{note}");
        assert!(note.contains("typing `harbor` runs this copy"), "{note}");

        // Running the Homebrew copy while PATH still reaches the script copy first.
        let this = Install::at(&brew, Some(&home)).unwrap();
        let note = describe(&this, &found, &version).unwrap();
        assert!(note.contains("(0.43.4, install.sh)"), "{note}");
        assert!(note.contains("install.sh | bash -s -- --uninstall"), "{note}");
        assert!(note.contains("not this copy"), "{note}");

        // One copy, however many ways PATH reaches it: nothing to say.
        let alone = distinct([script.clone(), script.clone()], Some(&home));
        let this = Install::at(&script, Some(&home)).unwrap();
        assert_eq!(describe(&this, &alone, &version), None);
        let _ = fs::remove_dir_all(root);
    }
}
