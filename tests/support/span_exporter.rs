//! In-memory stand-in for the OTLP network boundary.
use opentelemetry_sdk::{
    error::OTelSdkResult,
    trace::{SpanData, SpanExporter},
};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default)]
pub struct InMemorySpanExporter(Arc<Mutex<Vec<SpanData>>>);
impl SpanExporter for InMemorySpanExporter {
    async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
        self.0.lock().expect("span exporter").extend(batch);
        Ok(())
    }
}
impl InMemorySpanExporter {
    pub fn get_finished_spans(&self) -> Result<Vec<SpanData>, &'static str> {
        self.0.lock().map(|spans| spans.clone()).map_err(|_| "poisoned exporter")
    }
}
