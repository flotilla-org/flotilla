//! Injected host free-space measurement. Like `CommandRunner` and `EnvVars`,
//! callers receive this port from the composition root instead of querying the
//! operating system directly, so tests and other environments can substitute it.
use std::{path::Path, sync::Arc};

#[cfg(not(unix))]
use sysinfo::Disks;

pub trait AvailableSpaceProbe: Send + Sync {
    fn measure(&self, path: &Path) -> Option<u64>;
}

struct SystemAvailableSpaceProbe;

impl AvailableSpaceProbe for SystemAvailableSpaceProbe {
    fn measure(&self, path: &Path) -> Option<u64> {
        #[cfg(unix)]
        {
            target_available_space(path, |candidate| {
                use std::{ffi::CString, os::unix::ffi::OsStrExt};
                let candidate = CString::new(candidate.as_os_str().as_bytes())
                    .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
                let mut stats = std::mem::MaybeUninit::<libc::statvfs>::uninit();
                // SAFETY: candidate is NUL-terminated and stats is writable.
                if unsafe { libc::statvfs(candidate.as_ptr(), stats.as_mut_ptr()) } != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // SAFETY: successful statvfs initialized the output.
                let stats = unsafe { stats.assume_init() };
                // Casts are needed on 32-bit targets; u128 also prevents overflow.
                let bytes = u128::from(stats.f_bavail) * u128::from(stats.f_frsize);
                u64::try_from(bytes).map_err(std::io::Error::other)
            })
            .ok()
        }
        #[cfg(not(unix))]
        {
            let disks = Disks::new_with_refreshed_list();
            disks
                .list()
                .iter()
                .filter(|disk| path.starts_with(disk.mount_point()))
                .max_by_key(|disk| disk.mount_point().components().count())
                .map(|disk| disk.available_space())
        }
    }
}

/// Inspect only the requested filesystem, never enumerate other mounted
/// volumes (which can trigger macOS removable/network-volume privacy prompts).
/// A not-yet-created checkout inherits its nearest existing ancestor's space.
#[cfg(unix)]
fn target_available_space(path: &Path, mut query: impl FnMut(&Path) -> std::io::Result<u64>) -> std::io::Result<u64> {
    for candidate in path.ancestors() {
        match query(candidate) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            result => return result,
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no existing ancestor for space probe"))
}

/// Measure the operating system's available space for the requested path.
pub fn system_available_space_probe() -> Arc<dyn AvailableSpaceProbe> {
    Arc::new(SystemAvailableSpaceProbe)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generate nonexistent path depths through 8; only that path and its
    // ancestors may be inspected, never unrelated protected/removable volumes.
    #[cfg(unix)]
    #[hegel::test]
    fn space_probe_touches_only_target_ancestors(tc: hegel::TestCase) {
        let drawn_depth = tc.draw(hegel::generators::integers::<usize>().min_value(0).max_value(8));
        for depth in [0, 1, 8, drawn_depth] {
            let mut path = std::path::PathBuf::from("/workspace");
            for _ in 0..depth {
                path.push("new");
            }
            let mut touched = Vec::new();
            let result = target_available_space(&path, |candidate| {
                touched.push(candidate.to_path_buf());
                if candidate == Path::new("/workspace") {
                    Ok(123)
                } else {
                    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
                }
            });
            assert_eq!(result.expect("existing ancestor"), 123);
            assert_eq!(touched, path.ancestors().take(depth + 1).map(Path::to_path_buf).collect::<Vec<_>>());
            let mut calls = 0;
            assert!(target_available_space(&path, |_| {
                calls += 1;
                Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
            })
            .is_err());
            assert_eq!(calls, 1, "permission denial must not fall back to a different filesystem");
        }
    }
}
