use chrono::{DateTime, Utc};
use flotilla_resources::{ConditionValue, HostCondition};
use tracing::{info, warn};

const TARGET_NOFILE_SOFT_LIMIT: u64 = 8_192;
const FILE_DESCRIPTOR_PRESSURE_PERCENT: u64 = 80;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileDescriptorUsage {
    open: u64,
    soft_limit: u64,
}

pub(crate) fn raise_file_descriptor_limit() {
    #[cfg(unix)]
    if let Err(error) = raise_file_descriptor_limit_unix() {
        warn!(%error, "failed to raise daemon file descriptor limit");
    }
}

pub(crate) fn file_descriptor_pressure_condition() -> Option<HostCondition> {
    let usage = current_file_descriptor_usage()?;
    file_descriptor_pressure_condition_for(usage, Utc::now())
}

fn file_descriptor_pressure_condition_for(usage: FileDescriptorUsage, observed_at: DateTime<Utc>) -> Option<HostCondition> {
    if usage.soft_limit == 0 || usage.open.saturating_mul(100) < usage.soft_limit.saturating_mul(FILE_DESCRIPTOR_PRESSURE_PERCENT) {
        return None;
    }

    let percent = usage.open.saturating_mul(100) / usage.soft_limit;
    Some(
        HostCondition::builder()
            .condition_type("FileDescriptors")
            .value(ConditionValue::False)
            .reason("FileDescriptorPressure")
            .message(format!(
                "{} of {} file descriptors are open ({percent}%); connection acceptance is at risk",
                usage.open, usage.soft_limit
            ))
            .observed_at(observed_at)
            .build(),
    )
}

fn current_file_descriptor_usage() -> Option<FileDescriptorUsage> {
    #[cfg(unix)]
    {
        let soft_limit = nofile_limits().ok()?.0;
        let open = open_file_descriptor_count()?;
        Some(FileDescriptorUsage { open, soft_limit })
    }
    #[cfg(not(unix))]
    {
        None
    }
}

pub(crate) fn open_file_descriptor_count() -> Option<u64> {
    #[cfg(unix)]
    {
        ["/proc/self/fd", "/dev/fd"]
            .into_iter()
            .find_map(|path| std::fs::read_dir(path).ok().map(|entries| entries.filter_map(Result::ok).count() as u64))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

#[cfg(unix)]
fn nofile_limits() -> Result<(u64, u64), String> {
    let mut limits = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: `limits` points to a valid `rlimit` value for the duration of the call.
    let result = unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limits) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok((limits.rlim_cur, limits.rlim_max))
}

#[cfg(unix)]
fn raise_file_descriptor_limit_unix() -> Result<(), String> {
    let (current, hard) = nofile_limits()?;
    let desired = desired_nofile_soft_limit(current, hard);
    if desired <= current {
        info!(soft_limit = current, hard_limit = hard, "daemon file descriptor limit");
        return Ok(());
    }

    let limits = libc::rlimit { rlim_cur: desired, rlim_max: hard };
    // SAFETY: `limits` is initialized, and changing only this process's soft
    // RLIMIT_NOFILE to a value no greater than its hard limit is valid.
    let result = unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limits) };
    if result != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    info!(previous_soft_limit = current, soft_limit = desired, hard_limit = hard, "raised daemon file descriptor limit");
    Ok(())
}

fn desired_nofile_soft_limit(current: u64, hard: u64) -> u64 {
    current.max(TARGET_NOFILE_SOFT_LIMIT.min(hard))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn desired_limit_raises_stock_soft_limit_without_exceeding_hard_limit() {
        assert_eq!(desired_nofile_soft_limit(1_024, 1_048_576), 8_192);
        assert_eq!(desired_nofile_soft_limit(1_024, 4_096), 4_096);
        assert_eq!(desired_nofile_soft_limit(16_384, 1_048_576), 16_384);
    }

    #[test]
    fn file_descriptor_pressure_is_reported_at_eighty_percent() {
        let below = file_descriptor_pressure_condition_for(FileDescriptorUsage { open: 818, soft_limit: 1_024 }, Utc::now());
        assert!(below.is_none());

        let condition = file_descriptor_pressure_condition_for(FileDescriptorUsage { open: 820, soft_limit: 1_024 }, Utc::now())
            .expect("80% usage should be diagnosed");
        assert_eq!(condition.condition_type, "FileDescriptors");
        assert_eq!(condition.reason, "FileDescriptorPressure");
        assert!(condition.message.contains("820 of 1024"));
    }
}
