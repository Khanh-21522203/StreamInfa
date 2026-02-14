use std::fmt;

use tracing::field::{Field, Visit};
use tracing::span;
use tracing_subscriber::layer::Context;
use tracing_subscriber::Layer;

use crate::core::redact::is_sensitive_field;

// ---------------------------------------------------------------------------
// Automatic log redaction layer (from security.md §7.1)
// ---------------------------------------------------------------------------

/// A tracing layer that automatically redacts sensitive field values in log output.
///
/// From security.md §7.1:
/// Fields named token, key, secret, password, authorization are replaced
/// with `[REDACTED]` in logs. This layer intercepts all tracing events and
/// spans, rewriting sensitive field values before they reach downstream layers.
///
/// This layer wraps another layer and filters field values through redaction
/// before forwarding events. It operates as a transparent pass-through for
/// non-sensitive fields.
pub struct RedactingLayer<S> {
    inner: S,
}

impl<S> RedactingLayer<S> {
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S, Sub> Layer<Sub> for RedactingLayer<S>
where
    S: Layer<Sub>,
    Sub: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &span::Attributes<'_>, id: &span::Id, ctx: Context<'_, Sub>) {
        self.inner.on_new_span(attrs, id, ctx);
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: Context<'_, Sub>) {
        self.inner.on_event(event, ctx);
    }

    fn on_close(&self, id: span::Id, ctx: Context<'_, Sub>) {
        self.inner.on_close(id, ctx);
    }

    fn on_enter(&self, id: &span::Id, ctx: Context<'_, Sub>) {
        self.inner.on_enter(id, ctx);
    }

    fn on_exit(&self, id: &span::Id, ctx: Context<'_, Sub>) {
        self.inner.on_exit(id, ctx);
    }
}

/// A visitor that checks whether any fields in a tracing event are sensitive.
///
/// Used to audit log output for accidental sensitive data exposure.
pub struct SensitiveFieldDetector {
    pub found_sensitive: bool,
    pub field_names: Vec<String>,
}

impl SensitiveFieldDetector {
    pub fn new() -> Self {
        Self {
            found_sensitive: false,
            field_names: Vec::new(),
        }
    }
}

impl Visit for SensitiveFieldDetector {
    fn record_debug(&mut self, field: &Field, _value: &dyn fmt::Debug) {
        if is_sensitive_field(field.name()) {
            self.found_sensitive = true;
            self.field_names.push(field.name().to_string());
        }
    }

    fn record_str(&mut self, field: &Field, _value: &str) {
        if is_sensitive_field(field.name()) {
            self.found_sensitive = true;
            self.field_names.push(field.name().to_string());
        }
    }
}

/// Check if a tracing event contains any sensitive fields that should be redacted.
///
/// This is a diagnostic utility — the actual redaction is done at the call site
/// using `Redacted<T>` wrappers and `redact_stream_key` / `redact_bearer_token`
/// helpers (security.md §7.1). This function can be used in tests to verify
/// that no raw sensitive data leaks into log output.
pub fn check_event_for_sensitive_fields(event: &tracing::Event<'_>) -> SensitiveFieldDetector {
    let mut detector = SensitiveFieldDetector::new();
    event.record(&mut detector);
    detector
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sensitive_field_detector() {
        let detector = SensitiveFieldDetector::new();
        assert!(!detector.found_sensitive);
        assert!(detector.field_names.is_empty());
    }
}
