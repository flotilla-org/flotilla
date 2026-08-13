use std::{path::Path, sync::Arc};

use sysinfo::Disks;

const BYTES_PER_GIB: u64 = 1024 * 1024 * 1024;

pub(crate) fn free_space_floor_bytes(floor_gib: u64) -> Result<u64, String> {
    floor_gib.checked_mul(BYTES_PER_GIB).ok_or_else(|| format!("free-space floor is too large: {floor_gib} GiB"))
}

pub(crate) trait AvailableSpaceProbe: Send + Sync {
    fn measure(&self, path: &Path) -> Option<u64>;
}

struct SystemAvailableSpaceProbe;

impl AvailableSpaceProbe for SystemAvailableSpaceProbe {
    fn measure(&self, path: &Path) -> Option<u64> {
        let disks = Disks::new_with_refreshed_list();
        disks
            .list()
            .iter()
            .filter(|disk| path.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().components().count())
            .map(|disk| disk.available_space())
    }
}

pub(crate) fn system_available_space_probe() -> Arc<dyn AvailableSpaceProbe> {
    Arc::new(SystemAvailableSpaceProbe)
}

pub(crate) fn check_free_space_floor(probe: &dyn AvailableSpaceProbe, host: &str, path: &Path, floor_gib: u64) -> Result<(), String> {
    if floor_gib == 0 {
        return Ok(());
    }

    let floor_bytes =
        free_space_floor_bytes(floor_gib).map_err(|_| format!("free-space floor for host `{host}` is too large: {floor_gib} GiB"))?;
    let free_bytes = probe
        .measure(path)
        .ok_or_else(|| format!("placement refused on host `{host}`: free space could not be measured for {}", path.display()))?;
    check_measured_free_space(host, free_bytes, floor_bytes)
}

pub(crate) fn check_measured_free_space(host: &str, free_bytes: u64, floor_bytes: u64) -> Result<(), String> {
    if free_bytes >= floor_bytes {
        return Ok(());
    }

    Err(format!(
        "placement refused on host `{host}`: {} free is below the {} floor; reap settled convoys, run scripts/prune-target.sh, or pick another host",
        format_free_gib(free_bytes),
        format_gib(floor_bytes),
    ))
}

fn format_free_gib(bytes: u64) -> String {
    let tenths = u128::from(bytes) * 10 / u128::from(BYTES_PER_GIB);
    format!("{}.{:01} GiB", tenths / 10, tenths % 10)
}

fn format_gib(bytes: u64) -> String {
    let gib = bytes as f64 / BYTES_PER_GIB as f64;
    format!("{gib:.1} GiB")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedAvailableSpaceProbe(Option<u64>);

    impl AvailableSpaceProbe for FixedAvailableSpaceProbe {
        fn measure(&self, _path: &Path) -> Option<u64> {
            self.0
        }
    }

    #[test]
    fn injected_space_below_floor_is_refused_with_actionable_error() {
        let probe = FixedAvailableSpaceProbe(Some(12 * BYTES_PER_GIB));

        let result = check_free_space_floor(&probe, "kiwi", Path::new("/workspace"), 20);

        assert_eq!(
            result,
            Err("placement refused on host `kiwi`: 12.0 GiB free is below the 20.0 GiB floor; \
                 reap settled convoys, run scripts/prune-target.sh, or pick another host"
                .to_string())
        );
    }

    #[test]
    fn injected_space_equal_to_floor_is_admitted() {
        let probe = FixedAvailableSpaceProbe(Some(20 * BYTES_PER_GIB));

        assert_eq!(check_free_space_floor(&probe, "kiwi", Path::new("/workspace"), 20), Ok(()));
    }

    #[test]
    fn injected_unmeasurable_space_is_refused() {
        let probe = FixedAvailableSpaceProbe(None);

        assert_eq!(
            check_free_space_floor(&probe, "kiwi", Path::new("/workspace"), 20),
            Err("placement refused on host `kiwi`: free space could not be measured for /workspace".to_string())
        );
    }

    #[test]
    fn refusal_does_not_round_free_space_up_to_the_floor() {
        let just_below_floor = 20 * BYTES_PER_GIB - 1;

        let result = check_measured_free_space("kiwi", just_below_floor, 20 * BYTES_PER_GIB);

        assert!(result.expect_err("space below the floor should be refused").contains("19.9 GiB free is below the 20.0 GiB floor"));
    }

    #[test]
    fn zero_floor_disables_the_probe() {
        let probe = FixedAvailableSpaceProbe(None);

        assert_eq!(check_free_space_floor(&probe, "kiwi", Path::new("/path/that/does/not/exist"), 0), Ok(()));
    }
}
