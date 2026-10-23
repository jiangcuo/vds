//! Logging setup and helpers (see `docs/spec/logging.md`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::registry::Registry;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

use crate::error::{Error, Result};

/// Environment variable holding the default log filter.
pub const ENV_FILTER: &str = "VDS_LOG";
/// Environment variable holding the default log format (`text` or `json`).
pub const ENV_FORMAT: &str = "VDS_LOG_FORMAT";

/// Output format of log lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogFormat {
    /// One human-readable line per event (fixed format, see logging.md 1.5).
    #[default]
    Text,
    /// One JSON object per line.
    Json,
}

impl LogFormat {
    /// Parses `text` or `json`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "text" => Ok(Self::Text),
            "json" => Ok(Self::Json),
            other => Err(Error::InvalidConfig(format!("unknown log format `{other}`"))),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Default)]
pub struct LogConfig {
    /// `EnvFilter` directive; `None` falls back to `VDS_LOG`, then to `info`.
    pub filter: Option<String>,
    /// Output format; `None` falls back to `VDS_LOG_FORMAT`, then to text.
    pub format: Option<LogFormat>,
}

/// Installs the global `tracing` subscriber writing to stderr.
///
/// Must be called at most once per process, by the host (C ABI) or by a binary's `main`.
/// Libraries never call this on their own.
pub fn init(cfg: &LogConfig) -> Result<()> {
    let directive = match &cfg.filter {
        Some(f) => f.clone(),
        None => std::env::var(ENV_FILTER).unwrap_or_else(|_| String::from("info")),
    };
    let filter = EnvFilter::try_new(&directive)
        .map_err(|e| Error::InvalidConfig(format!("invalid log filter `{directive}`: {e}")))?;
    let format = match cfg.format {
        Some(f) => f,
        None => match std::env::var(ENV_FORMAT) {
            Ok(s) => LogFormat::parse(&s)?,
            Err(_) => LogFormat::Text,
        },
    };

    // One layer per destination: stderr keeps the configured filter, the debug directory gets
    // its own (untouched by `filter`, and writing nothing while no directory is configured).
    let stderr: Box<dyn Layer<Registry> + Send + Sync> = match format {
        LogFormat::Text => Box::new(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(filter),
        ),
        LogFormat::Json => Box::new(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .with_filter(filter),
        ),
    };
    let sessions = crate::debug::SessionLogLayer::new().with_filter(EnvFilter::new(crate::debug::LOG_FILTER));

    tracing_subscriber::registry()
        .with(stderr)
        .with(sessions)
        .try_init()
        .map_err(|_| Error::LogAlreadyInitialized)
}

/// Per-call-site rate limiter used by [`warn_ratelimited!`](crate::warn_ratelimited).
#[derive(Debug)]
pub struct RateLimiter {
    last: Mutex<Option<Instant>>,
    suppressed: AtomicU64,
}

impl RateLimiter {
    /// Creates a limiter that has never fired.
    pub const fn new() -> Self {
        Self {
            last: parking_lot::const_mutex(None),
            suppressed: AtomicU64::new(0),
        }
    }

    /// Returns `Some(suppressed_count)` if an event may be emitted now, `None` if it must be
    /// suppressed because one was emitted less than `period` ago.
    pub fn check(&self, period: Duration) -> Option<u64> {
        let now = Instant::now();
        let mut last = self.last.lock();
        match *last {
            Some(t) if now.duration_since(t) < period => {
                self.suppressed.fetch_add(1, Ordering::Relaxed);
                None
            }
            _ => {
                *last = Some(now);
                Some(self.suppressed.swap(0, Ordering::Relaxed))
            }
        }
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

/// `warn!` limited to one event per call site every 10 seconds.
///
/// The emitted event carries a `suppressed` field with the number of events dropped since the
/// previous one.
#[macro_export]
macro_rules! warn_ratelimited {
    ($($arg:tt)+) => {{
        static LIMITER: $crate::log::RateLimiter = $crate::log::RateLimiter::new();
        if let Some(suppressed) = LIMITER.check(::std::time::Duration::from_secs(10)) {
            $crate::__tracing::warn!(suppressed, $($arg)+);
        }
    }};
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{LogFormat, RateLimiter};

    #[test]
    fn limiter_suppresses_within_period() {
        let l = RateLimiter::new();
        assert_eq!(l.check(Duration::from_secs(60)), Some(0));
        assert_eq!(l.check(Duration::from_secs(60)), None);
        assert_eq!(l.check(Duration::from_secs(60)), None);
        assert_eq!(l.check(Duration::ZERO), Some(2));
    }

    #[test]
    fn parse_format() {
        assert_eq!(LogFormat::parse("json").unwrap(), LogFormat::Json);
        assert!(LogFormat::parse("xml").is_err());
    }
}
