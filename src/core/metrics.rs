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

/// Record the time spent waiting on a channel send (performance-and-backpressure.md §4.4).
pub fn record_channel_send_wait(channel_name: &str, wait_seconds: f64) {
    metrics::histogram!("streaminfa_channel_send_wait_seconds", "channel" => channel_name.to_string())
        .record(wait_seconds);
}

/// Increment backpressure event counter when a send blocks > 100ms.
pub fn inc_backpressure_event(channel_name: &str) {
    metrics::counter!("streaminfa_backpressure_events_total", "channel" => channel_name.to_string())
        .increment(1);
}

/// Backpressure threshold: sends blocking longer than this trigger a backpressure event.
const BACKPRESSURE_THRESHOLD_MS: u128 = 100;

/// Send a value on a bounded mpsc channel with backpressure metrics tracking.
///
/// Records channel utilization, send wait time, and backpressure events
/// per performance-and-backpressure.md §4.4.
pub async fn send_with_backpressure<T>(
    tx: &tokio::sync::mpsc::Sender<T>,
    value: T,
    channel_name: &str,
    capacity: usize,
) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
    // Record utilization before send
    let current_len = capacity.saturating_sub(tx.capacity());
    record_channel_metrics(channel_name, current_len, capacity);

    let start = std::time::Instant::now();
    let result = tx.send(value).await;
    let elapsed = start.elapsed();

    // Record send wait time
    record_channel_send_wait(channel_name, elapsed.as_secs_f64());

    // Track backpressure events (sends that blocked > 100ms)
    if elapsed.as_millis() > BACKPRESSURE_THRESHOLD_MS {
        inc_backpressure_event(channel_name);
    }

    // Record utilization after send
    let current_len = capacity.saturating_sub(tx.capacity());
    record_channel_metrics(channel_name, current_len, capacity);

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_utilization_tracking() {
        record_channel_metrics("test_chan", 4, 8);
        // Just verify it doesn't panic
    }

    #[tokio::test]
    async fn test_send_with_backpressure() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(4);
        let result = send_with_backpressure(&tx, 42, "test", 4).await;
        assert!(result.is_ok());
        assert_eq!(rx.recv().await, Some(42));
    }
}
