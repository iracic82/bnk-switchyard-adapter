//! Logging and tracing. JSON logs always; OTLP/HTTP traces when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set. libsy's `libsy.run` and `libsy.llm_call` spans nest
//! under the adapter's `chat_completion` span, so one trace shows the decision and the answer.

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

pub struct Telemetry {
    provider: Option<SdkTracerProvider>,
}

impl Telemetry {
    pub fn init() -> anyhow::Result<Self> {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("info,libsy=info,h2=warn,hyper=warn"));
        let fmt = tracing_subscriber::fmt::layer()
            .json()
            .with_current_span(true)
            .with_span_list(false);
        let provider = if std::env::var_os("OTEL_EXPORTER_OTLP_ENDPOINT").is_some() {
            let exporter = opentelemetry_otlp::SpanExporter::builder()
                .with_http()
                .build()?;
            let service = std::env::var("OTEL_SERVICE_NAME")
                .unwrap_or_else(|_| "bnk-switchyard-adapter".into());
            Some(
                SdkTracerProvider::builder()
                    .with_batch_exporter(exporter)
                    .with_resource(Resource::builder().with_service_name(service).build())
                    .build(),
            )
        } else {
            None
        };
        let otel = provider.as_ref().map(|provider| {
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("bnk-switchyard-adapter"))
                .with_filter(EnvFilter::new("info"))
        });
        tracing_subscriber::registry()
            .with(filter)
            .with(fmt)
            .with(otel)
            .try_init()?;
        Ok(Self { provider })
    }

    pub fn shutdown(self) {
        if let Some(provider) = self.provider {
            let _ = provider.shutdown();
        }
    }
}
