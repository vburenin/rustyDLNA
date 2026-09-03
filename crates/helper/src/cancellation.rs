//! Cooperative cancellation shared by long-running helper work.

/// Cloneable cooperative cancellation for one daemon lifetime.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl CancellationToken {
    /// Request cancellation. Repeated calls are harmless.
    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Atomic view for helper APIs that already consume a cancellation flag.
    pub fn as_atomic(&self) -> &std::sync::atomic::AtomicBool {
        &self.cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_observe_the_same_one_way_transition() {
        let first = CancellationToken::default();
        let second = first.clone();
        assert!(!first.is_cancelled());
        second.cancel();
        assert!(first.is_cancelled());
        assert!(second
            .as_atomic()
            .load(std::sync::atomic::Ordering::Acquire));
    }
}
