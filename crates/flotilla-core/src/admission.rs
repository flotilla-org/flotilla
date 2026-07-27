use std::path::Path;

use sysinfo::Disks;

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

pub(crate) fn check_free_space_floor(host: &str, path: &Path, floor_gib: u64) -> Result<(), String> {
    if floor_gib == 0 {
        return Ok(());
    }

    let floor_bytes =
        floor_gib.checked_mul(BYTES_PER_GIB).ok_or_else(|| format!("free-space floor for host `{host}` is too large: {floor_gib} GiB"))?;
    let free_bytes = available_space(path)
        .ok_or_else(|| format!("placement refused on host `{host}`: free space could not be measured for {}", path.display()))?;
    check_measured_free_space(host, free_bytes, floor_bytes)
}

fn available_space(path: &Path) -> Option<u64> {
    let disks = Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        .filter(|disk| path.starts_with(disk.mount_point()))
        .max_by_key(|disk| disk.mount_point().components().count())
        .map(|disk| disk.available_space())
}

fn check_measured_free_space(host: &str, free_bytes: u64, floor_bytes: u64) -> Result<(), String> {
    if free_bytes >= floor_bytes {
        return Ok(());
    }

    Err(format!(
        "placement refused on host `{host}`: {} free is below the {} floor; reap settled convoys, run scripts/prune-target.sh, or pick another host",
        format_gib(free_bytes),
        format_gib(floor_bytes),
    ))
}

fn format_gib(bytes: u64) -> String {
    let gib = bytes as f64 / BYTES_PER_GIB as f64;
    format!("{gib:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_floor_refusal_is_actionable() {
        let result = check_measured_free_space("kiwi", 12 * BYTES_PER_GIB, 20 * BYTES_PER_GIB);

        assert_eq!(
            result,
            Err("placement refused on host `kiwi`: 12.0 GiB free is below the 20.0 GiB floor; \
                 reap settled convoys, run scripts/prune-target.sh, or pick another host"
                .to_string())
        );
    }

    #[test]
    fn space_equal_to_floor_is_admitted() {
        assert_eq!(check_measured_free_space("kiwi", 20 * BYTES_PER_GIB, 20 * BYTES_PER_GIB), Ok(()));
    }

    #[test]
    fn zero_floor_disables_the_probe() {
        assert_eq!(check_free_space_floor("kiwi", Path::new("/path/that/does/not/exist"), 0), Ok(()));
    }
}
