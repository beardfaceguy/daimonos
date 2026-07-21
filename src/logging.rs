use std::path::Path;
use std::time::Duration;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::config::LoggingConfig;

/// Keeps the non-blocking file writer alive until process teardown.
pub struct LoggingGuard {
    _file_guard: WorkerGuard,
    started: std::time::Instant,
}

impl Drop for LoggingGuard {
    fn drop(&mut self) {
        tracing::info!(
            target: "daimonos::lifecycle",
            event = "process_stop",
            pid = std::process::id(),
            uptime_secs = self.started.elapsed().as_secs(),
        );
    }
}

pub fn init(config: &LoggingConfig) -> anyhow::Result<Option<LoggingGuard>> {
    if !config.enabled {
        return Ok(None);
    }

    let directory = config.resolved_directory();
    std::fs::create_dir_all(&directory)?;
    let rotation = match config.rotation.as_str() {
        "hourly" => Rotation::HOURLY,
        "daily" => Rotation::DAILY,
        "never" => Rotation::NEVER,
        other => anyhow::bail!("unsupported log rotation '{other}'"),
    };
    let appender = RollingFileAppender::builder()
        .rotation(rotation)
        .filename_prefix(&config.file_prefix)
        .max_log_files(config.max_files)
        .build(&directory)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(appender);

    let file_layer = fmt::layer()
        .json()
        .with_ansi(false)
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(file_writer)
        .with_filter(EnvFilter::new(config.level.clone()));
    let stderr_layer = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_filter(EnvFilter::new(config.stderr_level.clone()));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()?;

    Ok(Some(LoggingGuard {
        _file_guard: file_guard,
        started: std::time::Instant::now(),
    }))
}

/// Emit bounded, content-free process telemetry. CPU is reported as the
/// delta of Linux scheduler ticks since the prior sample; this makes runaway
/// processes visible without adding a platform-specific system-information
/// dependency. Unsupported platforms still report PID and uptime.
pub fn spawn_resource_telemetry(interval_secs: u64) -> Option<tokio::task::JoinHandle<()>> {
    if interval_secs == 0 {
        return None;
    }
    Some(tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(interval_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let started = std::time::Instant::now();
        let mut previous_ticks = None;
        loop {
            interval.tick().await;
            let snapshot = ProcessSnapshot::read();
            let cpu_delta_ticks = snapshot
                .cpu_ticks
                .zip(previous_ticks)
                .map(|(current, previous)| current.saturating_sub(previous));
            previous_ticks = snapshot.cpu_ticks;
            tracing::info!(
                target: "daimonos::telemetry",
                event = "process_resources",
                pid = std::process::id(),
                uptime_secs = started.elapsed().as_secs(),
                rss_kib = snapshot.rss_kib,
                threads = snapshot.threads,
                open_fds = snapshot.open_fds,
                cpu_ticks = snapshot.cpu_ticks,
                cpu_delta_ticks,
            );
        }
    }))
}

#[derive(Debug, Default, PartialEq, Eq)]
struct ProcessSnapshot {
    cpu_ticks: Option<u64>,
    rss_kib: Option<u64>,
    threads: Option<u64>,
    open_fds: Option<u64>,
}

impl ProcessSnapshot {
    fn read() -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                cpu_ticks: read_linux_cpu_ticks(Path::new("/proc/self/stat")),
                rss_kib: read_status_value(Path::new("/proc/self/status"), "VmRSS:"),
                threads: read_status_value(Path::new("/proc/self/status"), "Threads:"),
                open_fds: std::fs::read_dir("/proc/self/fd")
                    .ok()
                    .map(|entries| entries.filter_map(Result::ok).count() as u64),
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }
}

#[cfg(target_os = "linux")]
fn read_linux_cpu_ticks(path: &Path) -> Option<u64> {
    let stat = std::fs::read_to_string(path).ok()?;
    // comm is parenthesized and may contain spaces. Fields after its final ')'
    // begin with field 3 (state); utime/stime are fields 14/15.
    let after_comm = stat.rsplit_once(')')?.1.trim();
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    let utime = fields.get(11)?.parse::<u64>().ok()?;
    let stime = fields.get(12)?.parse::<u64>().ok()?;
    Some(utime.saturating_add(stime))
}

#[cfg(target_os = "linux")]
fn read_status_value(path: &Path, key: &str) -> Option<u64> {
    std::fs::read_to_string(path)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix(key))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_cpu_ticks_with_spaced_command_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stat");
        std::fs::write(
            &path,
            "42 (agent worker) S 1 2 3 4 5 6 7 8 9 10 100 25 0 0 0 0 0 0 0\n",
        )
        .unwrap();
        assert_eq!(read_linux_cpu_ticks(&path), Some(125));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_linux_status_values() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("status");
        std::fs::write(&path, "Name:\ttest\nVmRSS:\t12345 kB\nThreads:\t7\n").unwrap();
        assert_eq!(read_status_value(&path, "VmRSS:"), Some(12345));
        assert_eq!(read_status_value(&path, "Threads:"), Some(7));
    }
}
