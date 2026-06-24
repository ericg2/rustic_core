use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use crate::{ErrorKind, RusticError};

/// A lightweight cancellation token that can be cloned and shared across threads.
///
/// # Example
/// ```rust
/// let token = CancelToken::new();
/// let worker_token = token.clone();
///
/// std::thread::spawn(move || {
///     while !worker_token.is_cancelled() {
///         // do backup work...
///     }
///     println!("Worker stopped.");
/// });
///
/// token.cancel();
/// ```
#[derive(Clone, Debug)]
pub struct CancelToken {
    cancelled: Arc<AtomicBool>,
}

impl CancelToken {
    /// Creates a new, uncancelled token.
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Signals cancellation. All clones of this token will see the change immediately.
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// Returns `true` if cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// Returns `Ok(())` if not canceled, or [`RusticError`] if canceled.
    /// Useful for propagating cancellation with `?`.
    pub fn check(&self) -> Result<(), Box<RusticError>> {
        if self.is_cancelled() {
            Err(RusticError::new(ErrorKind::Other, "Operation canceled."))
        } else {
            Ok(())
        }
    }
}

impl Default for CancelToken {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;
    use crate::RusticResult;

    #[test]
    fn starts_uncancelled() {
        let token = CancelToken::new();
        assert!(!token.is_cancelled());
        assert!(token.check().is_ok());
    }

    #[test]
    fn cancel_is_visible() {
        let token = CancelToken::new();
        token.cancel();
        assert!(token.is_cancelled());
        assert!(token.check().is_err());
    }

    #[test]
    fn clones_share_state() {
        let token = CancelToken::new();
        let clone = token.clone();

        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled()); // clone sees the cancellation
    }

    #[test]
    fn cancel_from_another_thread() {
        let token = CancelToken::new();
        let sender = token.clone();

        let handle = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            sender.cancel();
        });

        // Spin until cancelled (or time out after 1 s)
        let start = std::time::Instant::now();
        loop {
            if token.is_cancelled() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(1), "timed out waiting for cancel");
            thread::sleep(Duration::from_millis(5));
        }

        handle.join().unwrap();
    }

    #[test]
    fn check_propagates_with_question_mark() {
        fn do_work(token: &CancelToken) -> RusticResult<()> {
            token.check()?;          // returns early if canceled
            Ok(())
        }

        let token = CancelToken::new();
        assert!(do_work(&token).is_ok());

        token.cancel();
        assert!(do_work(&token).is_err());
    }
}