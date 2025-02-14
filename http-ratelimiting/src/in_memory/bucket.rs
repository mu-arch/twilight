//! [`Bucket`] management used by the [`super::InMemoryRatelimiter`] internally.
//! Each bucket has an associated [`BucketQueue`] to queue an API request, which is
//! consumed by the [`BucketQueueTask`] that manages the ratelimit for the bucket
//! and respects the global ratelimit.

use super::GlobalLockPair;
use crate::{headers::RatelimitHeaders, request::Path, ticket::TicketNotifier};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        Mutex as AsyncMutex,
    },
    time::{sleep, timeout},
};

/// Time remaining until a bucket will reset.
#[derive(Clone, Debug)]
pub enum TimeRemaining {
    /// Bucket has already reset.
    Finished,
    /// Bucket's ratelimit refresh countdown has not started yet.
    NotStarted,
    /// Amount of time until the bucket resets.
    Some(Duration),
}

/// Ratelimit information for a bucket used in the [`super::InMemoryRatelimiter`].
///
/// A generic version not specific to this ratelimiter is [`crate::Bucket`].
#[derive(Debug)]
pub struct Bucket {
    /// Total number of tickets allotted in a cycle.
    pub limit: AtomicU64,
    /// Path this ratelimit applies to.
    pub path: Path,
    /// Queue associated with this bucket.
    pub queue: BucketQueue,
    /// Number of tickets remaining.
    pub remaining: AtomicU64,
    /// Duration after the [`Self::started_at`] time the bucket will refresh.
    pub reset_after: AtomicU64,
    /// When the bucket's ratelimit refresh countdown started.
    pub started_at: Mutex<Option<Instant>>,
}

impl Bucket {
    /// Create a new bucket for the specified [`Path`].
    pub fn new(path: Path) -> Self {
        Self {
            limit: AtomicU64::new(u64::max_value()),
            path,
            queue: BucketQueue::default(),
            remaining: AtomicU64::new(u64::max_value()),
            reset_after: AtomicU64::new(u64::max_value()),
            started_at: Mutex::new(None),
        }
    }

    /// Total number of tickets allotted in a cycle.
    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    /// Number of tickets remaining.
    pub fn remaining(&self) -> u64 {
        self.remaining.load(Ordering::Relaxed)
    }

    /// Duration after the [`started_at`] time the bucket will refresh.
    ///
    /// [`started_at`]: Self::started_at
    pub fn reset_after(&self) -> u64 {
        self.reset_after.load(Ordering::Relaxed)
    }

    /// Time remaining until this bucket will reset.
    pub fn time_remaining(&self) -> TimeRemaining {
        let reset_after = self.reset_after();
        let started_at = match *self.started_at.lock().expect("bucket poisoned") {
            Some(v) => v,
            None => return TimeRemaining::NotStarted,
        };
        let elapsed = started_at.elapsed();

        if elapsed > Duration::from_millis(reset_after) {
            return TimeRemaining::Finished;
        }

        TimeRemaining::Some(Duration::from_millis(reset_after) - elapsed)
    }

    /// Try to reset this bucket's [`started_at`] value if it has finished.
    ///
    /// Returns whether resetting was possible.
    ///
    /// [`started_at`]: Self::started_at
    pub fn try_reset(&self) -> bool {
        if self.started_at.lock().expect("bucket poisoned").is_none() {
            return false;
        }

        if let TimeRemaining::Finished = self.time_remaining() {
            self.remaining.store(self.limit(), Ordering::Relaxed);
            *self.started_at.lock().expect("bucket poisoned") = None;

            true
        } else {
            false
        }
    }

    /// Update this bucket's ratelimit data after a request has been made.
    pub fn update(&self, ratelimits: Option<(u64, u64, u64)>) {
        let bucket_limit = self.limit();

        {
            let mut started_at = self.started_at.lock().expect("bucket poisoned");

            if started_at.is_none() {
                started_at.replace(Instant::now());
            }
        }

        if let Some((limit, remaining, reset_after)) = ratelimits {
            if bucket_limit != limit && bucket_limit == u64::max_value() {
                self.reset_after.store(reset_after, Ordering::SeqCst);
                self.limit.store(limit, Ordering::SeqCst);
            }

            self.remaining.store(remaining, Ordering::Relaxed);
        } else {
            self.remaining.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// Queue of ratelimit requests for a bucket.
#[derive(Debug)]
pub struct BucketQueue {
    /// Receiver for the ratelimit requests.
    rx: AsyncMutex<UnboundedReceiver<TicketNotifier>>,
    /// Sender for the ratelimit requests.
    tx: UnboundedSender<TicketNotifier>,
}

impl BucketQueue {
    /// Add a new ratelimit request to the queue.
    pub fn push(&self, tx: TicketNotifier) {
        let _sent = self.tx.send(tx);
    }

    /// Receive the first incoming ratelimit request.
    pub async fn pop(&self, timeout_duration: Duration) -> Option<TicketNotifier> {
        let mut rx = self.rx.lock().await;

        timeout(timeout_duration, rx.recv()).await.ok().flatten()
    }
}

impl Default for BucketQueue {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        Self {
            rx: AsyncMutex::new(rx),
            tx,
        }
    }
}

/// A background task that handles ratelimit requests to a [`Bucket`]
/// and processes them in order, keeping track of both the global and
/// the [`Path`]-specific ratelimits.
pub(super) struct BucketQueueTask {
    /// The [`Bucket`] managed by this task.
    bucket: Arc<Bucket>,
    /// All buckets managed by the associated [`super::InMemoryRatelimiter`].
    buckets: Arc<Mutex<HashMap<Path, Arc<Bucket>>>>,
    /// Global ratelimit data.
    global: Arc<GlobalLockPair>,
    /// The [`Path`] this [`Bucket`] belongs to.
    path: Path,
}

impl BucketQueueTask {
    /// Timeout to wait for response headers after initiating a request.
    const WAIT: Duration = Duration::from_secs(10);

    /// Create a new task to manage the ratelimit for a [`Bucket`].
    pub fn new(
        bucket: Arc<Bucket>,
        buckets: Arc<Mutex<HashMap<Path, Arc<Bucket>>>>,
        global: Arc<GlobalLockPair>,
        path: Path,
    ) -> Self {
        Self {
            bucket,
            buckets,
            global,
            path,
        }
    }

    /// Process incoming ratelimit requests to this bucket and update the state
    /// based on received [`RatelimitHeaders`].
    pub async fn run(self) {
        #[cfg(feature = "tracing")]
        let span = tracing::debug_span!("background queue task", path=?self.path);

        while let Some(queue_tx) = self.next().await {
            if self.global.is_locked() {
                self.global.0.lock().await;
            }

            let ticket_headers = if let Some(ticket_headers) = queue_tx.available() {
                ticket_headers
            } else {
                continue;
            };

            #[cfg(feature = "tracing")]
            tracing::debug!(parent: &span, "starting to wait for response headers",);

            match timeout(Self::WAIT, ticket_headers).await {
                Ok(Ok(Some(headers))) => self.handle_headers(&headers).await,
                Ok(Ok(None)) => {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(parent: &span, "request aborted");
                }
                Ok(Err(_)) => {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(parent: &span, "ticket channel closed");
                }
                Err(_) => {
                    #[cfg(feature = "tracing")]
                    tracing::debug!(parent: &span, "receiver timed out");
                }
            }
        }

        #[cfg(feature = "tracing")]
        tracing::debug!(parent: &span, "bucket appears finished, removing");

        self.buckets
            .lock()
            .expect("ratelimit buckets poisoned")
            .remove(&self.path);
    }

    /// Update the bucket's ratelimit state.
    async fn handle_headers(&self, headers: &RatelimitHeaders) {
        let ratelimits = match headers {
            RatelimitHeaders::GlobalLimited(global_limited) => {
                self.lock_global(Duration::from_secs(global_limited.retry_after()))
                    .await;

                None
            }
            RatelimitHeaders::None => return,
            RatelimitHeaders::Present(present) => {
                Some((present.limit(), present.remaining(), present.reset_after()))
            }
        };

        #[cfg(feature = "tracing")]
        tracing::debug!(path=?self.path, "updating bucket");
        self.bucket.update(ratelimits);
    }

    /// Lock the global ratelimit for a specified duration.
    async fn lock_global(&self, wait: Duration) {
        #[cfg(feature = "tracing")]
        tracing::debug!(path=?self.path, "request got global ratelimited");
        self.global.lock();
        let lock = self.global.0.lock().await;
        sleep(wait).await;
        self.global.unlock();

        drop(lock);
    }

    /// Get the next [`TicketNotifier`] in the queue.
    async fn next(&self) -> Option<TicketNotifier> {
        #[cfg(feature = "tracing")]
        tracing::debug!(path=?self.path, "starting to get next in queue");

        self.wait_if_needed().await;

        self.bucket.queue.pop(Self::WAIT).await
    }

    /// Wait for this bucket to refresh if it isn't ready yet.
    async fn wait_if_needed(&self) {
        #[cfg(feature = "tracing")]
        let span = tracing::debug_span!("waiting for bucket to refresh", path=?self.path);

        let wait = {
            if self.bucket.remaining() > 0 {
                return;
            }

            #[cfg(feature = "tracing")]
            tracing::debug!(parent: &span, "0 tickets remaining, may have to wait");

            match self.bucket.time_remaining() {
                TimeRemaining::Finished => {
                    self.bucket.try_reset();

                    return;
                }
                TimeRemaining::NotStarted => return,
                TimeRemaining::Some(dur) => dur,
            }
        };

        #[cfg(feature = "tracing")]
        tracing::debug!(
            parent: &span,
            milliseconds=%wait.as_millis(),
            "waiting for ratelimit to pass",
        );

        sleep(wait).await;

        #[cfg(feature = "tracing")]
        tracing::debug!(parent: &span, "done waiting for ratelimit to pass");

        self.bucket.try_reset();
    }
}
