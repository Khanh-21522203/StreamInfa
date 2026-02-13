// ---------------------------------------------------------------------------
// Channel metrics (from performance-and-backpressure.md §4)
// ---------------------------------------------------------------------------

/// Describe all backpressure-related metrics (called once at startup).
pub fn describe_backpressure_metrics() {
    metrics::describe_histogram!(
        "streaminfa_channel_send_wait_seconds",
        "Time blocked on channel send"
    );
    metrics::describe_counter!(
        "streaminfa_backpressure_events_total",
        "Sends that blocked > 100ms"
    );
    metrics::describe_gauge!(
        "streaminfa_channel_utilization",
        "Channel fullness ratio (0.0-1.0)"
    );
}

/// Record current channel utilization for a given channel.
pub fn record_channel_metrics(label: &str, current_len: usize, capacity: usize) {
    let utilization = if capacity > 0 {
        current_len as f64 / capacity as f64
    } else {
        0.0
    };
    metrics::gauge!("streaminfa_channel_utilization", "channel" => label.to_string())
        .set(utilization);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_utilization_tracking() {
        record_channel_metrics("test_chan", 4, 8);
        // Just verify it doesn't panic
    }
}
