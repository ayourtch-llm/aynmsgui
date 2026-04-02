use std::collections::HashMap;
use tokio::sync::broadcast;
use serde::Serialize;

#[derive(Clone, Debug, Serialize)]
pub struct SseEvent {
    pub event_type: String,
    pub data: String,
}

pub struct OperationTracker {
    operations: HashMap<String, broadcast::Sender<SseEvent>>,
}

impl OperationTracker {
    pub fn new() -> Self {
        Self { operations: HashMap::new() }
    }

    /// Register a new operation. Returns (operation_id, sender).
    pub fn create_operation(&mut self) -> (String, broadcast::Sender<SseEvent>) {
        let id = uuid::Uuid::new_v4().to_string();
        let (tx, _) = broadcast::channel(64);
        self.operations.insert(id.clone(), tx.clone());
        (id, tx)
    }

    /// Subscribe to an operation's events.
    pub fn subscribe(&self, operation_id: &str) -> Option<broadcast::Receiver<SseEvent>> {
        self.operations.get(operation_id).map(|tx| tx.subscribe())
    }

    /// Remove a completed operation.
    pub fn remove_operation(&mut self, operation_id: &str) {
        self.operations.remove(operation_id);
    }
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
}
