//! A wake-up signal usable from both blocking threads and async tasks.

use std::time::Duration;

use parking_lot::{Condvar, Mutex};
use tokio::sync::Notify;

/// Level-triggered signal: [`Signal::raise`] sets a flag and wakes all waiters; waiting
/// consumes the flag.
///
/// Used to wake encoder threads (blocking) and protocol tasks (async) when new damage,
/// cursor changes or acknowledgements arrive.
#[derive(Debug, Default)]
pub struct Signal {
    flag: Mutex<bool>,
    cond: Condvar,
    notify: Notify,
}

impl Signal {
    /// Creates a lowered signal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Raises the signal and wakes every waiter.
    pub fn raise(&self) {
        *self.flag.lock() = true;
        self.cond.notify_all();
        // `notify_one` stores a permit, so an async waiter that has checked the flag but not
        // yet started waiting does not miss this wake-up.
        self.notify.notify_one();
    }

    /// Lowers the signal and returns whether it was raised.
    pub fn take(&self) -> bool {
        core::mem::replace(&mut *self.flag.lock(), false)
    }

    /// Blocks until the signal is raised or `timeout` elapses. Returns `true` if raised.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let mut flag = self.flag.lock();
        if !*flag {
            let _ = self.cond.wait_for(&mut flag, timeout);
        }
        core::mem::replace(&mut *flag, false)
    }

    /// Waits asynchronously until the signal is raised.
    pub async fn wait(&self) {
        loop {
            if self.take() {
                return;
            }
            self.notify.notified().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::Signal;

    #[test]
    fn raise_before_wait_is_not_lost() {
        let s = Signal::new();
        s.raise();
        assert!(s.wait_timeout(Duration::from_millis(1)));
        assert!(!s.wait_timeout(Duration::from_millis(1)));
    }

    #[test]
    fn wakes_blocking_waiter() {
        let s = Arc::new(Signal::new());
        let s2 = Arc::clone(&s);
        let t = std::thread::spawn(move || s2.wait_timeout(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(20));
        s.raise();
        assert!(t.join().unwrap());
    }

    #[tokio::test]
    async fn wakes_async_waiter() {
        let s = Arc::new(Signal::new());
        let s2 = Arc::clone(&s);
        let t = tokio::spawn(async move { s2.wait().await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        s.raise();
        tokio::time::timeout(Duration::from_secs(5), t).await.unwrap().unwrap();
    }
}
