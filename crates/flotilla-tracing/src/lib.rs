//! Process-local tracing context. No context belongs in a stored resource.
use std::{
    cell::Cell,
    fmt,
    sync::{Arc, OnceLock},
};

use opentelemetry::{
    propagation::{Extractor, Injector, TextMapPropagator},
    trace::{SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
    Context,
};
use opentelemetry_sdk::{
    propagation::TraceContextPropagator,
    trace::{IdGenerator, RandomIdGenerator},
};
use sha2::{Digest, Sha256};
use tracing::Span;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use tracing_subscriber::{
    fmt::{
        format::{FormatEvent, FormatFields, Writer},
        FmtContext,
    },
    registry::LookupSpan,
    Layer,
};

pub use opentelemetry::{trace::TracerProvider, KeyValue};
pub use opentelemetry_sdk::{
    trace::{Sampler, SdkTracerProvider},
    Resource,
};
pub use tracing_opentelemetry::layer;

thread_local! {
    static ROOT_ID: Cell<Option<(TraceId, SpanId)>> = const { Cell::new(None) };
}

/// The SDK still owns span creation; this generator supplies the stable queue root.
#[derive(Debug, Default)]
pub struct QueueIdGenerator(RandomIdGenerator);
impl IdGenerator for QueueIdGenerator {
    fn new_trace_id(&self) -> TraceId {
        ROOT_ID.with(|id| id.get().map(|id| id.0)).unwrap_or_else(|| self.0.new_trace_id())
    }
    fn new_span_id(&self) -> SpanId {
        ROOT_ID.with(|id| id.get().map(|id| id.1)).unwrap_or_else(|| self.0.new_span_id())
    }
}

fn queued_ids(identity: &str) -> (TraceId, SpanId) {
    let digest = Sha256::digest([b"flotilla.queued.v1\0".as_slice(), identity.as_bytes()].concat());
    let mut trace: [u8; 16] = digest[..16].try_into().expect("digest trace width");
    let mut span: [u8; 8] = digest[16..24].try_into().expect("digest span width");
    // W3C reserves all-zero IDs.
    if trace == [0; 16] {
        trace[15] = 1;
    }
    if span == [0; 8] {
        span[7] = 1;
    }
    (TraceId::from_bytes(trace), SpanId::from_bytes(span))
}

/// Admission is the deterministic root; subsequent phases parent to that root
/// even after a restart or a move to another host. The identity must be globally
/// qualified when the caller does not already have a globally stable ID.
pub fn queued_span(identity: &str, phase: &'static str) -> Span {
    let identity = identity.rsplit_once("/message:").map_or(identity, |(_, id)| id);
    let ids = queued_ids(identity);
    if phase == "admission" {
        struct Restore(Option<(TraceId, SpanId)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                ROOT_ID.with(|id| id.set(self.0));
            }
        }
        let _restore = Restore(ROOT_ID.with(|id| id.replace(Some(ids))));
        let span = tracing::info_span!(parent: None, "message.admission", message.identity = identity);
        // Force lazy SDK creation while the deterministic generator is scoped.
        let _ = span.context();
        span
    } else {
        let span = tracing::info_span!(parent: None, "message.phase", message.identity = identity, message.phase = phase);
        let parent = SpanContext::new(ids.0, ids.1, TraceFlags::SAMPLED, true, TraceState::default());
        let _ = span.set_parent(Context::new().with_remote_span_context(parent));
        span
    }
}

/// W3C carrier for transient wire envelopes only.
pub type WireContext = std::collections::BTreeMap<String, String>;
struct Carrier<'a>(&'a WireContext);
impl Extractor for Carrier<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(String::as_str).collect()
    }
}
struct CarrierMut<'a>(&'a mut WireContext);
impl Injector for CarrierMut<'_> {
    fn set(&mut self, key: &str, value: String) {
        self.0.insert(key.to_owned(), value);
    }
}
pub fn inject() -> WireContext {
    let mut carrier = WireContext::new();
    TraceContextPropagator::new().inject_context(&Span::current().context(), &mut CarrierMut(&mut carrier));
    carrier
}
pub fn receive(carrier: &WireContext) -> Span {
    let span = tracing::info_span!(parent: None, "peer.receive");
    let context = TraceContextPropagator::new().extract_with_context(&Context::new(), &Carrier(carrier));
    let _ = span.set_parent(context);
    span
}

/// Preserve the existing JSON event schema and add top-level correlation IDs.
struct JsonWithTrace(Arc<OnceLock<tracing::dispatcher::WeakDispatch>>);

struct TraceDispatch(Arc<OnceLock<tracing::dispatcher::WeakDispatch>>);
impl<S: tracing::Subscriber> Layer<S> for TraceDispatch {
    fn on_register_dispatch(&self, dispatch: &tracing::Dispatch) {
        let _ = self.0.set(dispatch.downgrade());
    }
}

/// A JSON log layer that resolves IDs through its own subscriber, including
/// scoped dispatchers and explicitly parented events. Keeping only a weak
/// dispatch avoids a subscriber/formatter reference cycle.
pub fn json_layer<S, W>(writer: W) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> tracing_subscriber::fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    let dispatch = Arc::new(OnceLock::new());
    TraceDispatch(Arc::clone(&dispatch))
        .and_then(tracing_subscriber::fmt::layer().json().event_format(JsonWithTrace(dispatch)).with_ansi(false).with_writer(writer))
}
impl<S, N> FormatEvent<S, N> for JsonWithTrace
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(&self, ctx: &FmtContext<'_, S, N>, mut writer: Writer<'_>, event: &tracing::Event<'_>) -> fmt::Result {
        let mut json = String::new();
        tracing_subscriber::fmt::format().json().format_event(ctx, Writer::new(&mut json), event)?;
        let mut value: serde_json::Value = serde_json::from_str(&json).map_err(|_| fmt::Error)?;
        let span = event.parent().and_then(|id| ctx.span(id)).or_else(|| if event.is_contextual() { ctx.lookup_current() } else { None });
        if let Some(span) = span {
            let context = self
                .0
                .get()
                .and_then(|dispatch| dispatch.upgrade())
                .and_then(|dispatch| tracing_opentelemetry::get_otel_context(&span.id(), &dispatch))
                .unwrap_or_default();
            let otel = context.span();
            let ids = otel.span_context();
            if ids.is_valid() {
                value["trace_id"] = ids.trace_id().to_string().into();
                value["span_id"] = ids.span_id().to_string().into();
            }
        }
        writeln!(writer, "{}", value)
    }
}

#[cfg(test)]
mod tests {
    use super::span_exporter::InMemorySpanExporter;
    use super::*;
    use hegel::generators as gs;
    use tracing_subscriber::layer::SubscriberExt;

    // Issue #3712: stable identities produce the same queue root on independent
    // hosts; distinct identities remain separate. Generate empty through long
    // identities, duplicate admissions and different sampling probabilities.
    #[hegel::test]
    fn queue_identity_survives_host_changes(tc: hegel::TestCase) {
        let size = tc.draw(gs::integers::<usize>().min_value(0).max_value(300));
        let suffix = tc.draw(gs::integers::<u64>());
        let identity = format!("{}-{suffix}", "é".repeat(size));
        let probability = [0.0, 0.5, 1.0][tc.draw(gs::integers::<usize>().min_value(0).max_value(2))];
        let provider = || {
            SdkTracerProvider::builder()
                .with_id_generator(QueueIdGenerator::default())
                .with_sampler(Sampler::TraceIdRatioBased(probability))
                .build()
        };
        let a = provider();
        let b = provider();
        let root = tracing::subscriber::with_default(tracing_subscriber::registry().with(layer().with_tracer(a.tracer("a"))), || {
            queued_span(&identity, "admission").context().span().span_context().clone()
        });
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer().with_tracer(b.tracer("b"))), || {
            let duplicate = queued_span(&identity, "admission").context();
            assert_eq!(root, *duplicate.span().span_context());
            let phase = queued_span(&identity, "attempt").context();
            assert_eq!(root.trace_id(), phase.span().span_context().trace_id());
            assert_eq!(root.is_sampled(), phase.span().span_context().is_sampled());
            assert_ne!(root.span_id(), phase.span().span_context().span_id());
            let other = queued_span(&format!("{identity}-other"), "admission").context();
            assert_ne!(root.trace_id(), other.span().span_context().trace_id());
            let regular = tracing::info_span!(parent: None, "ordinary").context();
            assert_ne!(root.trace_id(), regular.span().span_context().trace_id(), "queue ID scope cannot leak");
        });
    }

    // W3C propagation is transient: injection/extraction preserves a parent's
    // trace and connects a receiver span; invalid context starts a fresh trace.
    #[test]
    fn wire_context_round_trip_and_invalid_context() {
        let exporter = InMemorySpanExporter::default();
        let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
        tracing::subscriber::with_default(tracing_subscriber::registry().with(layer().with_tracer(provider.tracer("wire"))), || {
            let root = tracing::info_span!("request");
            let carrier = root.in_scope(inject);
            assert!(carrier.contains_key("traceparent"));
            let child = receive(&carrier);
            assert_eq!(root.context().span().span_context().trace_id(), child.context().span().span_context().trace_id());
            let invalid = root.in_scope(|| receive(&WireContext::from([("traceparent".into(), "invalid".into())])));
            assert_ne!(root.context().span().span_context().trace_id(), invalid.context().span().span_context().trace_id());
        });
        let spans = exporter.get_finished_spans().expect("spans");
        let root = spans.iter().find(|s| s.name == "request").expect("root");
        assert!(spans.iter().any(|s| s.parent_span_id == root.span_context.span_id()));
    }
}

#[cfg(test)]
#[path = "../../../tests/support/span_exporter.rs"]
mod span_exporter;

#[cfg(test)]
mod json_tests {
    use super::*;
    use std::{
        io,
        sync::{Arc, Mutex},
    };
    use tracing_subscriber::layer::SubscriberExt;

    #[derive(Clone, Default)]
    struct Logs(Arc<Mutex<Vec<u8>>>);
    impl io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("logs").extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    // Issue #3712: JSON logs inside spans always identify the actual event's
    // parent, including explicit parents, unsampled spans and export disabled.
    #[test]
    fn json_logs_identify_contextual_and_explicit_parents_without_export() {
        for sampling in [0.0, 1.0] {
            let provider = SdkTracerProvider::builder().with_sampler(Sampler::TraceIdRatioBased(sampling)).build();
            let logs = Logs::default();
            let writer = logs.clone();
            let subscriber =
                tracing_subscriber::registry().with(layer().with_tracer(provider.tracer("json"))).with(json_layer(move || writer.clone()));
            tracing::subscriber::with_default(subscriber, || {
                let parent = tracing::info_span!("parent");
                let expected = parent.context().span().span_context().clone();
                parent.in_scope(|| tracing::info!("contextual"));
                let other = tracing::info_span!("other");
                other.in_scope(|| tracing::info!(parent: &parent, "explicit"));
                tracing::info!("outside");
                let bytes = logs.0.lock().expect("logs");
                let lines: Vec<serde_json::Value> =
                    std::str::from_utf8(&bytes).expect("UTF8").lines().map(|line| serde_json::from_str(line).expect("JSON")).collect();
                for line in &lines[..2] {
                    assert_eq!(line["trace_id"], expected.trace_id().to_string());
                    assert_eq!(line["span_id"], expected.span_id().to_string());
                }
                assert!(lines[2].get("trace_id").is_none());
                assert!(lines[2].get("span_id").is_none());
            });
        }
    }
}
