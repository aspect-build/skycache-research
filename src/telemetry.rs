//! Logs to stderr, and metrics over OTLP when `OTEL_EXPORTER_OTLP_ENDPOINT` is set.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::global;
use opentelemetry_otlp::{MetricExporter, WithExportConfig};
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::metrics::{PeriodicReader, SdkMeterProvider};
use tracing_subscriber::EnvFilter;

static METER_PROVIDER: OnceLock<SdkMeterProvider> = OnceLock::new();

/// Must run inside the Tokio runtime: the OTLP exporter's channel is spawned on it.
pub fn install(service_name: &'static str) {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // An empty value means unset, so a deployment can switch export off explicitly.
    if !std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").is_ok_and(|v| !v.is_empty()) {
        tracing::info!("OTEL_EXPORTER_OTLP_ENDPOINT is unset; metrics are not exported");
        return;
    }
    let exporter = match MetricExporter::builder()
        .with_tonic()
        .with_timeout(Duration::from_secs(5))
        .build()
    {
        Ok(exporter) => exporter,
        Err(error) => {
            tracing::error!(%error, "building the OTLP metric exporter; metrics are not exported");
            return;
        }
    };
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(exporter).build())
        .with_resource(
            Resource::builder()
                .with_attributes([KeyValue::new("service.name", service_name)])
                .build(),
        )
        .build();
    if METER_PROVIDER.set(provider.clone()).is_ok() {
        global::set_meter_provider(provider);
    }
}

/// Flushes metrics recorded since the last export.
pub fn shutdown() {
    if let Some(provider) = METER_PROVIDER.get()
        && let Err(error) = provider.shutdown()
    {
        tracing::warn!(%error, "flushing metrics on shutdown");
    }
}
