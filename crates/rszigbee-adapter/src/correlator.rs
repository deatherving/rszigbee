//! Cancel-safe request/response correlation.
//!
//! Upstream solves this with `Waitress` and `oneWaitress` — a registry of
//! predicate/timeout pairs — because JavaScript has no structured cancellation:
//! an abandoned promise stays registered until it times out.
//!
//! Rust does have structured cancellation, so this is a map of keys to
//! [`oneshot::Sender`]s and the whole design reduces to one property:
//! **dropping the future removes the entry.** A caller that stops caring
//! (a `select!` branch that lost, a cancelled `tokio::time::timeout`, a task
//! that was aborted) leaves nothing behind. That is what makes every `async fn`
//! in the public API cancel-safe, which the README lists as a requirement.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::oneshot;

/// A pending-request registry keyed by `K`, resolving to `V`.
#[derive(Debug)]
pub struct Correlator<K: Eq + Hash + Clone, V> {
    inner: Arc<Mutex<HashMap<K, oneshot::Sender<V>>>>,
}

impl<K: Eq + Hash + Clone, V> Clone for Correlator<K, V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<K: Eq + Hash + Clone + core::fmt::Debug, V> Default for Correlator<K, V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: Eq + Hash + Clone + core::fmt::Debug, V> Correlator<K, V> {
    /// A new empty correlator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Registers interest in `key` and returns a guard that yields the value.
    ///
    /// Registering the same key twice replaces the first waiter, whose receiver
    /// then resolves to `Cancelled`. That is deliberate: a duplicate key means
    /// a transaction sequence number was reused, and silently delivering the
    /// response to the wrong caller is worse than failing the older one.
    pub fn register(&self, key: K) -> Pending<K, V> {
        let (tx, rx) = oneshot::channel();
        if let Ok(mut map) = self.inner.lock() {
            map.insert(key.clone(), tx);
        }
        Pending {
            key,
            rx: Some(rx),
            owner: self.clone(),
        }
    }

    /// Delivers a value to whoever is waiting for `key`.
    ///
    /// Returns the value back to the caller when nobody was waiting, so the
    /// adapter can decide what an unsolicited response means (usually: emit it
    /// as an event rather than discard it).
    pub fn resolve(&self, key: &K, value: V) -> Option<V> {
        let sender = self.inner.lock().ok().and_then(|mut m| m.remove(key));
        match sender {
            Some(tx) => tx.send(value).err(),
            None => Some(value),
        }
    }

    /// Number of outstanding registrations. Exposed because a monotonically
    /// growing correlator is the signature of a leak, and it deserves a metric.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.inner.lock().map_or(0, |m| m.len())
    }

    fn remove(&self, key: &K) {
        if let Ok(mut map) = self.inner.lock() {
            map.remove(key);
        }
    }
}

/// A registered interest in one response. Deregisters on drop.
#[derive(Debug)]
pub struct Pending<K: Eq + Hash + Clone + core::fmt::Debug, V> {
    key: K,
    rx: Option<oneshot::Receiver<V>>,
    owner: Correlator<K, V>,
}

/// Why a wait ended without a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum WaitError {
    /// The registration was replaced or the correlator was dropped.
    #[error("the pending request was cancelled")]
    Cancelled,
    /// Already awaited once.
    #[error("the pending request was already consumed")]
    Consumed,
}

impl<K: Eq + Hash + Clone + core::fmt::Debug, V> Pending<K, V> {
    /// The key this is waiting on.
    pub const fn key(&self) -> &K {
        &self.key
    }

    /// Waits for the response.
    pub async fn wait(mut self) -> Result<V, WaitError> {
        let rx = self.rx.take().ok_or(WaitError::Consumed)?;
        // On both paths the Drop impl removes the registration, so an abandoned
        // or failed wait cannot leak an entry.
        rx.await.map_err(|_| WaitError::Cancelled)
    }
}

impl<K: Eq + Hash + Clone + core::fmt::Debug, V> Drop for Pending<K, V> {
    fn drop(&mut self) {
        self.owner.remove(&self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn a_response_reaches_its_waiter() {
        let c: Correlator<u8, &'static str> = Correlator::new();
        let pending = c.register(7);
        assert_eq!(c.pending(), 1);
        assert!(c.resolve(&7, "readRsp").is_none());
        assert_eq!(pending.wait().await.unwrap(), "readRsp");
    }

    #[tokio::test]
    async fn dropping_the_future_deregisters_it() {
        // This is the entire reason this type exists. A caller that gave up
        // must not leave an entry behind for the next reuse of that sequence
        // number to resolve against.
        let c: Correlator<u8, u32> = Correlator::new();
        {
            let _pending = c.register(1);
            assert_eq!(c.pending(), 1);
        }
        assert_eq!(c.pending(), 0);
        // Nobody is waiting, so the value comes back to the caller.
        assert_eq!(c.resolve(&1, 99), Some(99));
    }

    #[tokio::test]
    async fn a_cancelled_timeout_leaves_nothing_behind() {
        tokio::time::pause();
        let c: Correlator<u8, u32> = Correlator::new();
        let pending = c.register(3);
        let waited = tokio::time::timeout(Duration::from_millis(50), pending.wait());
        tokio::time::advance(Duration::from_millis(100)).await;
        assert!(waited.await.is_err(), "should have timed out");
        assert_eq!(c.pending(), 0, "timing out must deregister");
    }

    #[tokio::test]
    async fn an_unsolicited_response_is_handed_back_not_swallowed() {
        // An adapter needs to distinguish "nobody asked for this" from
        // "delivered", because unsolicited frames are events, not garbage.
        let c: Correlator<u8, &'static str> = Correlator::new();
        assert_eq!(c.resolve(&42, "attributeReport"), Some("attributeReport"));
    }

    #[tokio::test]
    async fn a_duplicate_key_fails_the_older_waiter_rather_than_misrouting() {
        let c: Correlator<u8, u32> = Correlator::new();
        let first = c.register(5);
        let second = c.register(5);
        assert_eq!(c.pending(), 1);
        assert!(c.resolve(&5, 1).is_none());
        // The replaced waiter is told it was cancelled; it does not receive a
        // response that was not meant for it.
        assert_eq!(first.wait().await, Err(WaitError::Cancelled));
        assert_eq!(second.wait().await, Ok(1));
    }

    #[tokio::test]
    async fn correlators_are_shareable_across_tasks() {
        let c: Correlator<u8, u32> = Correlator::new();
        let pending = c.register(1);
        let c2 = c.clone();
        let h = tokio::spawn(async move {
            c2.resolve(&1, 7);
        });
        assert_eq!(pending.wait().await.unwrap(), 7);
        h.await.unwrap();
    }
}
