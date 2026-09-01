//! File modes, and the one question worth asking about a config file: could
//! anyone but me have written this?

use std::io::Write;
use std::path::Path;

/// True if someone other than us could swap this path's contents out from
/// under us: group- or world-writable, or owned by another user. A sticky
/// directory (`/tmp`) is writable by all but hijackable by none, so it passes.
///
/// Both binaries need this, and harbor needs it more than it looks. A berth
/// definition carries `init` SQL, and `init` can `LOAD` a native extension —
/// so a config file anyone can rewrite is not a settings leak, it is code
/// execution as its owner the next time a berth starts. Same argument
/// `token-cmd` already settled, same answer: refuse the file whole.
#[cfg(unix)]
pub fn exposed(path: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(md) = std::fs::metadata(path) else {
        return false;
    };
    let mode = md.permissions().mode();
    let sticky = md.is_dir() && mode & 0o1000 != 0;
    let uid = unsafe { libc::getuid() };
    (mode & 0o022 != 0 && !sticky) || (md.uid() != uid && md.uid() != 0)
}

#[cfg(not(unix))]
pub fn exposed(_path: &Path) -> bool {
    false
}

/// Create a file that is 0600 from its first byte, and write `contents`.
///
/// The point is the absence of a window: `fs::write` followed by `chmod` is
/// correct at rest and wrong in between, and "in between" is where a secret
/// leaks.
pub fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(contents.as_bytes())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
    }
}

/// Create a directory that is 0700 from the moment it exists. Same reasoning
/// as `write_private`, for the directory the tokens live in: `create_dir_all`
/// applies the umask, so the plain form is 0755 for the instant before a
/// chmod — and in that window another local user can plant a `<name>.token`
/// this process would then adopt as its own credential.
pub fn create_dir_private(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(path)
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
    }
}

pub fn chmod(path: &Path, mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
        Ok(())
    }
}

/// Ensure a directory exists, is ours alone, and stays that way.
pub fn ensure_private_dir(path: &Path) -> Result<(), String> {
    if !path.exists() {
        create_dir_private(path).map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    }
    let _ = chmod(path, 0o700);
    Ok(())
}
