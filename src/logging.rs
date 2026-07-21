use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::config::LoggingConfig;

struct SecureRollingAppender {
    inner: RollingFileAppender,
    directory: PathBuf,
    file_prefix: String,
}

impl Write for SecureRollingAppender {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        secure_log_files(&self.directory, &self.file_prefix)?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

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
    secure_log_directory(&directory)?;
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
    let appender = SecureRollingAppender {
        inner: appender,
        directory: directory.clone(),
        file_prefix: config.file_prefix.clone(),
    };
    secure_log_files(&directory, &config.file_prefix)?;
    let (file_writer, file_guard) = tracing_appender::non_blocking(appender);

    let file_layer = fmt::layer()
        .json()
        .with_ansi(false)
        .with_current_span(false)
        .with_span_list(false)
        .with_writer(file_writer)
        .with_filter(daimonos_filter(&config.level));
    let stderr_layer = fmt::layer()
        .compact()
        .with_ansi(false)
        .with_writer(std::io::stderr)
        .with_filter(daimonos_filter(&config.stderr_level));

    tracing_subscriber::registry()
        .with(file_layer)
        .with(stderr_layer)
        .try_init()?;

    Ok(Some(LoggingGuard {
        _file_guard: file_guard,
        started: std::time::Instant::now(),
    }))
}

fn daimonos_filter(level: &str) -> EnvFilter {
    EnvFilter::new(format!("off,daimonos={level}"))
}

#[cfg(unix)]
fn secure_log_directory(directory: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn secure_log_directory(_directory: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn secure_log_files(directory: &Path, file_prefix: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with(file_prefix)
            || !entry.file_type()?.is_file()
        {
            continue;
        }
        std::fs::set_permissions(entry.path(), std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn secure_log_files(_directory: &Path, _file_prefix: &str) -> std::io::Result<()> {
    Ok(())
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
    use std::sync::{Arc, Mutex};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    struct CapturedGuard(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedGuard {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturedWriter {
        type Writer = CapturedGuard;

        fn make_writer(&'a self) -> Self::Writer {
            CapturedGuard(Arc::clone(&self.0))
        }
    }

    #[test]
    fn excludes_dependency_events_from_logs() {
        let output = CapturedWriter::default();
        let subscriber = tracing_subscriber::registry().with(
            fmt::layer()
                .with_writer(output.clone())
                .with_filter(daimonos_filter("info")),
        );

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "daimonos::test", event = "safe_event");
            tracing::error!(
                target: "dependency::transport",
                message = "Authorization: Bearer secret-value"
            );
        });

        let captured = String::from_utf8(
            output
                .0
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        )
        .unwrap();
        assert!(captured.contains("safe_event"));
        assert!(!captured.contains("secret-value"));
        assert!(!captured.contains("dependency::transport"));
    }

    #[cfg(unix)]
    #[test]
    fn secures_log_directory_and_matching_files() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("logs");
        std::fs::create_dir(&directory).unwrap();
        let log = directory.join("daimonos.test");
        let unrelated = directory.join("other.test");
        std::fs::write(&log, "log").unwrap();
        std::fs::write(&unrelated, "other").unwrap();

        secure_log_directory(&directory).unwrap();
        secure_log_files(&directory, "daimonos").unwrap();

        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_ne!(
            std::fs::metadata(&unrelated).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

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
