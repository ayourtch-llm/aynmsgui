use std::collections::HashMap;
use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

/// Status of an operation.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub enum OperationStatus {
    Running,
    Complete,
    Error,
}

/// Metadata for a tracked operation.
#[derive(Clone, Debug)]
pub struct OperationInfo {
    pub id: String,
    pub op_type: String,
    pub device: String,
    pub status: OperationStatus,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_message: String,
}

struct OperationEntry {
    info: OperationInfo,
    tx: broadcast::Sender<SseEvent>,
}

pub struct OperationTracker {
    operations: HashMap<String, OperationEntry>,
}

impl OperationTracker {
    pub fn new() -> Self {
        Self { operations: HashMap::new() }
    }

    /// Register a new operation with metadata. Returns (operation_id, sender).
    pub fn create_operation_with_info(
        &mut self,
        op_type: &str,
        device: &str,
    ) -> (String, broadcast::Sender<SseEvent>) {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, _) = broadcast::channel(64);
        let info = OperationInfo {
            id: id.clone(),
            op_type: op_type.to_string(),
            device: device.to_string(),
            status: OperationStatus::Running,
            started_at: Utc::now(),
            finished_at: None,
            last_message: String::new(),
        };
        self.operations.insert(id.clone(), OperationEntry { info, tx: tx.clone() });
        (id, tx)
    }

    /// Backwards-compatible: create with no metadata.
    pub fn create_operation(&mut self) -> (String, broadcast::Sender<SseEvent>) {
        self.create_operation_with_info("unknown", "")
    }

    /// Subscribe to an operation's events.
    pub fn subscribe(&self, operation_id: &str) -> Option<broadcast::Receiver<SseEvent>> {
        self.operations.get(operation_id).map(|entry| entry.tx.subscribe())
    }

    /// Update the last message for an operation.
    pub fn update_message(&mut self, operation_id: &str, message: &str) {
        if let Some(entry) = self.operations.get_mut(operation_id) {
            entry.info.last_message = message.to_string();
        }
    }

    /// Mark an operation as complete.
    pub fn complete_operation(&mut self, operation_id: &str, message: &str) {
        if let Some(entry) = self.operations.get_mut(operation_id) {
            entry.info.status = OperationStatus::Complete;
            entry.info.finished_at = Some(Utc::now());
            entry.info.last_message = message.to_string();
        }
    }

    /// Mark an operation as failed.
    pub fn fail_operation(&mut self, operation_id: &str, message: &str) {
        if let Some(entry) = self.operations.get_mut(operation_id) {
            entry.info.status = OperationStatus::Error;
            entry.info.finished_at = Some(Utc::now());
            entry.info.last_message = message.to_string();
        }
    }

    /// Remove a completed/failed operation (call after some retention time).
    pub fn remove_operation(&mut self, operation_id: &str) {
        self.operations.remove(operation_id);
    }

    /// List all operations (running first, then recent completed/failed).
    pub fn list_operations(&self) -> Vec<OperationInfo> {
        let mut ops: Vec<OperationInfo> = self.operations
            .values()
            .map(|entry| entry.info.clone())
            .collect();
        // Running first, then by start time descending
        ops.sort_by(|a, b| {
            let a_running = a.status == OperationStatus::Running;
            let b_running = b.status == OperationStatus::Running;
            b_running.cmp(&a_running).then_with(|| b.started_at.cmp(&a.started_at))
        });
        ops
    }

    /// Clean up completed/failed operations older than `max_age`.
    pub fn cleanup_old(&mut self, max_age: std::time::Duration) {
        let cutoff = Utc::now() - chrono::Duration::from_std(max_age).unwrap_or(chrono::Duration::hours(1));
        self.operations.retain(|_, entry| {
            match entry.info.finished_at {
                Some(t) if t < cutoff => false,
                _ => true,
            }
        });
    }
}

/// Generate the SSE-connected progress page HTML.
///
/// This is the shared component used by all background operation handlers.
pub fn sse_progress_page(title: &str, details: &str, op_id: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head><meta charset="UTF-8"><title>{title}</title>
<link rel="stylesheet" type="text/css" href="/static/css/site.css" />
</head>
<body>
<div class="main">
<h1>{title}</h1>
{details}
<p>Operation: <code>{op_id}</code></p>
<div id="progress"></div>
<script>
const evtSource = new EventSource("/operations/{op_id}/stream");
const div = document.getElementById("progress");
evtSource.addEventListener("progress", function(e) {{
    div.innerHTML += "<p>" + e.data + "</p>";
    window.scrollTo(0, document.body.scrollHeight);
}});
evtSource.addEventListener("complete", function(e) {{
    div.innerHTML += "<p style='color:green'><strong>Complete:</strong> " + e.data + "</p>";
    evtSource.close();
}});
evtSource.addEventListener("error", function(e) {{
    if (e.data) {{
        div.innerHTML += "<p style='color:red'><strong>Error:</strong> " + e.data + "</p>";
    }}
    evtSource.close();
}});
</script>
<p><a href="/operations">All Operations</a> | <a href="/">Dashboard</a></p>
</div>
</body>
</html>"#,
        title = title,
        details = details,
        op_id = op_id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_operation() {
        let mut tracker = OperationTracker::new();
        let (id, _tx) = tracker.create_operation();
        assert!(!id.is_empty(), "operation id should be non-empty");
    }

    #[test]
    fn test_create_operation_with_info() {
        let mut tracker = OperationTracker::new();
        let (id, _tx) = tracker.create_operation_with_info("upgrade", "switch-01");
        let ops = tracker.list_operations();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].id, id);
        assert_eq!(ops[0].op_type, "upgrade");
        assert_eq!(ops[0].device, "switch-01");
        assert_eq!(ops[0].status, OperationStatus::Running);
    }

    #[tokio::test]
    async fn test_subscribe_to_operation() {
        let mut tracker = OperationTracker::new();
        let (id, tx) = tracker.create_operation();

        let mut rx = tracker.subscribe(&id).expect("subscribe should return Some");

        let event = SseEvent {
            event_type: "test".to_string(),
            data: "hello".to_string(),
        };
        tx.send(event.clone()).expect("send should succeed");

        let received = rx.recv().await.expect("recv should succeed");
        assert_eq!(received.event_type, "test");
        assert_eq!(received.data, "hello");
    }

    #[test]
    fn test_subscribe_to_nonexistent_returns_none() {
        let tracker = OperationTracker::new();
        let result = tracker.subscribe("nonexistent-id");
        assert!(result.is_none(), "subscribing to unknown id should return None");
    }

    #[test]
    fn test_remove_operation() {
        let mut tracker = OperationTracker::new();
        let (id, _tx) = tracker.create_operation();
        tracker.remove_operation(&id);
        let result = tracker.subscribe(&id);
        assert!(result.is_none(), "after removal, subscribe should return None");
    }

    #[test]
    fn test_complete_and_fail_operations() {
        let mut tracker = OperationTracker::new();
        let (id1, _) = tracker.create_operation_with_info("upgrade", "sw1");
        let (id2, _) = tracker.create_operation_with_info("extract", "sw2");

        tracker.complete_operation(&id1, "done");
        tracker.fail_operation(&id2, "timeout");

        let ops = tracker.list_operations();
        assert_eq!(ops.len(), 2);
        // Both should be finished
        let op1 = ops.iter().find(|o| o.id == id1).unwrap();
        assert_eq!(op1.status, OperationStatus::Complete);
        let op2 = ops.iter().find(|o| o.id == id2).unwrap();
        assert_eq!(op2.status, OperationStatus::Error);
    }
}
