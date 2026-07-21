use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

use crate::config::LoggingConfig;

#[derive(Clone, Copy)]
enum LogRotation {
    Hourly,
    Daily,
    Never,
}

impl LogRotation {
    fn bucket(self) -> u64 {
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        match self {
            Self::Hourly => seconds / 3_600,
            Self::Daily => seconds / 86_400,
            Self::Never => 0,
        }
    }
}

struct SecureRollingAppender {
    directory: PathBuf,
    file_prefix: String,
    rotation: LogRotation,
    max_files: usize,
    bucket: u64,
    file: File,
}

impl SecureRollingAppender {
    fn new(
        directory: PathBuf,
        file_prefix: String,
        rotation: LogRotation,
        max_files: usize,
    ) -> std::io::Result<Self> {
        secure_log_directory(&directory)?;
        let bucket = rotation.bucket();
        let current_path = log_path(&directory, &file_prefix, rotation, bucket);
        let file = open_secure_log(&current_path)?;
        prune_log_files(&directory, &file_prefix, max_files, &current_path)?;
        Ok(Self {
            directory,
            file_prefix,
            rotation,
            max_files,
            bucket,
            file,
        })
    }

    fn rotate_if_needed(&mut self) -> std::io::Result<()> {
        let bucket = self.rotation.bucket();
        if bucket == self.bucket {
            return Ok(());
        }
        let current_path = log_path(&self.directory, &self.file_prefix, self.rotation, bucket);
        self.file = open_secure_log(&current_path)?;
        self.bucket = bucket;
        prune_log_files(
            &self.directory,
            &self.file_prefix,
            self.max_files,
            &current_path,
        )
    }
}

impl Write for SecureRollingAppender {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.rotate_if_needed()?;
        self.file.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
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
    let rotation = match config.rotation.as_str() {
        "hourly" => LogRotation::Hourly,
        "daily" => LogRotation::Daily,
        "never" => LogRotation::Never,
        other => anyhow::bail!("unsupported log rotation '{other}'"),
    };
    let appender = SecureRollingAppender::new(
        directory,
        config.file_prefix.clone(),
        rotation,
        config.max_files,
    )?;
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
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::fs::PermissionsExt;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn secure_log_directory(directory: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(directory)
}

fn open_secure_log(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

fn log_path(directory: &Path, file_prefix: &str, rotation: LogRotation, bucket: u64) -> PathBuf {
    let filename = match rotation {
        LogRotation::Never => file_prefix.to_string(),
        LogRotation::Hourly | LogRotation::Daily => format!("{file_prefix}.{bucket}"),
    };
    directory.join(filename)
}

fn prune_log_files(
    directory: &Path,
    file_prefix: &str,
    max_files: usize,
    current_path: &Path,
) -> std::io::Result<()> {
    let rotated_prefix = format!("{file_prefix}.");
    let mut files = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let filename = entry.file_name();
        let filename = filename.to_string_lossy();
        if (filename == file_prefix || filename.starts_with(&rotated_prefix))
            && entry.file_type()?.is_file()
            && entry.path() != current_path
        {
            files.push((
                entry
                    .metadata()?
                    .modified()
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                entry.path(),
            ));
        }
    }
    files.sort_by_key(|(modified, _)| *modified);
    let remove_count = files.len().saturating_add(1).saturating_sub(max_files);
    for (_, path) in files.into_iter().take(remove_count) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

#[cfg(test)]
fn log_file_mode(path: &Path) -> std::io::Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(0)
    }
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
    fn creates_private_log_directory_and_file() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("logs");
        let mut appender = SecureRollingAppender::new(
            directory.clone(),
            "daimonos".to_string(),
            LogRotation::Never,
            2,
        )
        .unwrap();
        appender.write_all(b"test event\n").unwrap();

        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(log_file_mode(&directory.join("daimonos")).unwrap(), 0o600);
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
