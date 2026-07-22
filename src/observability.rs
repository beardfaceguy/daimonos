use std::collections::HashMap;
use std::time::Duration;

use base64::Engine;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry::KeyValue;
use opentelemetry_otlp::{WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::trace::{
    BatchConfigBuilder, BatchSpanProcessor, Sampler, SdkTracer, SdkTracerProvider, SpanData,
    SpanExporter,
};
use opentelemetry_sdk::Resource;

use crate::config::ObservabilityConfig;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservabilityStatus {
    Disabled,
    Active,
    Failed(String),
}

pub struct ObservabilityRuntime {
    provider: Option<SdkTracerProvider>,
    status: ObservabilityStatus,
    flush_timeout: Duration,
}

impl ObservabilityRuntime {
    pub fn initialize(config: &ObservabilityConfig) -> Self {
        let flush_timeout = Duration::from_millis(config.flush_timeout_ms);
        if !config.enabled {
            return Self {
                provider: None,
                status: ObservabilityStatus::Disabled,
                flush_timeout,
            };
        }

        let (username, password) = match config.resolve_basic_auth() {
            Ok(credentials) => credentials,
            Err(error) => {
                return Self {
                    provider: None,
                    status: ObservabilityStatus::Failed(error),
                    flush_timeout,
                };
            }
        };
        let exporter = match build_exporter(config, &username, &password) {
            Ok(exporter) => exporter,
            Err(error) => {
                return Self {
                    provider: None,
                    status: ObservabilityStatus::Failed(format!(
                        "OTLP exporter initialization failed: {error}"
                    )),
                    flush_timeout,
                };
            }
        };
        let provider = build_provider(config, LoggingExporter(exporter));

        Self {
            provider: Some(provider),
            status: ObservabilityStatus::Active,
            flush_timeout,
        }
    }

    pub fn status(&self) -> &ObservabilityStatus {
        &self.status
    }

    #[cfg(test)]
    pub fn tracer_provider(&self) -> Option<&SdkTracerProvider> {
        self.provider.as_ref()
    }

    pub fn tracer(&self) -> Option<SdkTracer> {
        self.provider
            .as_ref()
            .map(|provider| provider.tracer("daimonos"))
    }

    pub fn shutdown(&mut self) {
        if let Some(provider) = self.provider.take() {
            if provider.shutdown_with_timeout(self.flush_timeout).is_err() {
                tracing::warn!(
                    target: "daimonos::observability",
                    event = "telemetry_shutdown_failed",
                    timeout_ms = self.flush_timeout.as_millis() as u64,
                );
            }
        }
    }
}

impl Drop for ObservabilityRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[derive(Debug)]
struct LoggingExporter<E>(E);

impl<E: SpanExporter> SpanExporter for LoggingExporter<E> {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        let result = self.0.export(batch).await;
        if result.is_err() {
            tracing::warn!(
                target: "daimonos::observability",
                event = "telemetry_export_failed",
            );
        }
        result
    }

    fn shutdown_with_timeout(&self, timeout: Duration) -> OTelSdkResult {
        self.0.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> OTelSdkResult {
        self.0.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.0.set_resource(resource);
    }
}

fn build_provider<E: SpanExporter + 'static>(
    config: &ObservabilityConfig,
    exporter: E,
) -> SdkTracerProvider {
    let batch = BatchConfigBuilder::default()
        .with_max_queue_size(config.max_queue_size)
        .with_max_export_batch_size(config.max_batch_size)
        .with_scheduled_delay(Duration::from_millis(config.batch_delay_ms))
        .build();
    let processor = BatchSpanProcessor::builder(exporter)
        .with_batch_config(batch)
        .build();
    let sampler = Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(config.sample_ratio)));
    let mut resource = Resource::builder()
        .with_service_name("daimonos")
        .with_attribute(KeyValue::new(
            "deployment.environment.name",
            config.environment.clone(),
        ))
        .with_attribute(KeyValue::new("service.version", env!("CARGO_PKG_VERSION")));
    if let Some(release) = config.release.as_deref() {
        resource = resource.with_attribute(KeyValue::new("langfuse.release", release.to_string()));
    }
    SdkTracerProvider::builder()
        .with_sampler(sampler)
        .with_resource(resource.build())
        .with_span_processor(processor)
        .build()
}

fn build_exporter(
    config: &ObservabilityConfig,
    username: &str,
    password: &str,
) -> Result<opentelemetry_otlp::SpanExporter, opentelemetry_otlp::ExporterBuildError> {
    let headers = HashMap::from([(
        "authorization".to_string(),
        basic_authorization_header(username, password),
    )]);
    opentelemetry_otlp::SpanExporter::builder()
        .with_http()
        .with_endpoint(config.endpoint.clone())
        .with_headers(headers)
        .build()
}

fn basic_authorization_header(username: &str, password: &str) -> String {
    let credentials = format!("{username}:{password}");
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(credentials)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ObservabilityConfig;
    use opentelemetry::trace::{Span as _, Tracer as _};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::sync::Mutex;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn disabled_runtime_does_not_resolve_credentials() {
        let config = ObservabilityConfig {
            enabled: false,
            basic_auth_username_env: "DAIMONOS_TEST_MISSING_PUBLIC".to_string(),
            basic_auth_password_env: "DAIMONOS_TEST_MISSING_SECRET".to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        assert_eq!(runtime.status(), &ObservabilityStatus::Disabled);
        assert!(runtime.tracer_provider().is_none());
    }

    #[test]
    fn missing_credentials_fail_open_without_secret_material() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
        let username_env = "DAIMONOS_TEST_MISSING_PUBLIC";
        let password_env = "DAIMONOS_TEST_MISSING_SECRET";
        std::env::remove_var(username_env);
        std::env::remove_var(password_env);
        let config = ObservabilityConfig {
            enabled: true,
            basic_auth_username_env: username_env.to_string(),
            basic_auth_password_env: password_env.to_string(),
            ..ObservabilityConfig::default()
        };

        let runtime = ObservabilityRuntime::initialize(&config);

        let ObservabilityStatus::Failed(message) = runtime.status() else {
            panic!("missing credentials should disable export");
        };
        assert!(message.contains(username_env));
        assert!(!message.contains("Authorization"));
        assert!(runtime.tracer_provider().is_none());
    }

    #[test]
    fn basic_authorization_header_matches_rfc_7617() {
        assert_eq!(
            basic_authorization_header("public", "secret"),
            "Basic cHVibGljOnNlY3JldA=="
        );
    }

    #[derive(Debug)]
    struct SlowExporter {
        exports: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl SpanExporter for SlowExporter {
        async fn export(&self, _batch: Vec<SpanData>) -> OTelSdkResult {
            self.exports.fetch_add(1, Ordering::Relaxed);
            std::thread::sleep(self.delay);
            Ok(())
        }
    }

    #[test]
    fn saturated_queue_drops_spans_without_blocking_producer() {
        let config = ObservabilityConfig {
            max_queue_size: 1,
            max_batch_size: 1,
            batch_delay_ms: 1,
            ..ObservabilityConfig::default()
        };
        let exports = Arc::new(AtomicUsize::new(0));
        let provider = build_provider(
            &config,
            SlowExporter {
                exports: Arc::clone(&exports),
                delay: Duration::from_millis(500),
            },
        );
        let tracer = provider.tracer("queue-test");
        let started = std::time::Instant::now();
        for _ in 0..1_000 {
            let mut span = tracer.start("queued");
            span.end();
        }

        assert!(
            started.elapsed() < Duration::from_millis(100),
            "span producers must not wait for exporter backpressure"
        );
        assert!(provider
            .shutdown_with_timeout(Duration::from_millis(20))
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn exports_otlp_http_with_basic_auth() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let username_env = "DAIMONOS_TEST_OTLP_PUBLIC";
        let password_env = "DAIMONOS_TEST_OTLP_SECRET";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
            String::from_utf8_lossy(&request).into_owned()
        });
        let mut runtime = {
            let _guard = ENV_MUTEX.lock().unwrap_or_else(|error| error.into_inner());
            std::env::set_var(username_env, "public");
            std::env::set_var(password_env, "secret");
            let config = ObservabilityConfig {
                enabled: true,
                endpoint: format!("http://{address}/api/public/otel/v1/traces"),
                basic_auth_username_env: username_env.to_string(),
                basic_auth_password_env: password_env.to_string(),
                batch_delay_ms: 10,
                flush_timeout_ms: 2_000,
                ..ObservabilityConfig::default()
            };
            let runtime = ObservabilityRuntime::initialize(&config);
            std::env::remove_var(username_env);
            std::env::remove_var(password_env);
            runtime
        };
        assert_eq!(runtime.status(), &ObservabilityStatus::Active);
        let mut span = runtime.tracer().unwrap().start("export-test");
        span.end();
        runtime.shutdown();
        let request = tokio::time::timeout(Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        assert!(request.starts_with("POST /api/public/otel/v1/traces "));
        assert!(request
            .to_ascii_lowercase()
            .contains("authorization: basic chvibgljonnly3jlda=="));
    }
}
