//! Write buffer: batches positions into the database and replays them after
//! a database outage.
//!
//! * Rows are queued in a bounded FIFO. When full, the **oldest** rows are
//!   dropped (and counted): recent data is more valuable for a live system.
//! * Every `flush_interval` (or when a batch is full) the front of the queue is
//!   inserted. Rows leave the queue only after a successful insert, so a
//!   failing database loses nothing within the capacity.
//! * Failed inserts back off exponentially up to `max_backoff`.
//! * On shutdown, a final flush runs with a deadline.

use std::collections::VecDeque;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use serde::Serialize;
use storage::{Db, PositionRow};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// Destination of buffered rows (the database, or a fake in tests).
pub trait PositionSink: Send + Sync {
    /// Inserts all rows or none.
    fn insert(&self, rows: &[PositionRow]) -> impl Future<Output = Result<(), String>> + Send;
}

impl PositionSink for Db {
    async fn insert(&self, rows: &[PositionRow]) -> Result<(), String> {
        self.insert_positions(rows)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Buffer tuning.
#[derive(Debug, Clone, Copy)]
pub struct BufferConfig {
    pub capacity: usize,
    pub batch_size: usize,
    pub flush_interval: Duration,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub shutdown_deadline: Duration,
}

impl BufferConfig {
    /// Production defaults with the given capacity and shutdown deadline.
    #[must_use]
    pub fn new(capacity: usize, shutdown_deadline: Duration) -> Self {
        Self {
            capacity: capacity.max(1),
            batch_size: 1_000,
            flush_interval: Duration::from_millis(500),
            initial_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(10),
            shutdown_deadline,
        }
    }
}

/// Counters shared with the health endpoint.
#[derive(Debug, Default)]
pub struct BufferStats {
    queued: AtomicUsize,
    written: AtomicU64,
    dropped: AtomicU64,
    failed_flushes: AtomicU64,
}

/// Snapshot of [`BufferStats`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BufferSnapshot {
    pub queued: usize,
    pub written: u64,
    pub dropped: u64,
    pub failed_flushes: u64,
}

impl BufferStats {
    /// Current values.
    #[must_use]
    pub fn snapshot(&self) -> BufferSnapshot {
        BufferSnapshot {
            queued: self.queued.load(Ordering::Relaxed),
            written: self.written.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            failed_flushes: self.failed_flushes.load(Ordering::Relaxed),
        }
    }
}

struct WriteBuffer<S> {
    sink: S,
    cfg: BufferConfig,
    queue: VecDeque<PositionRow>,
    stats: Arc<BufferStats>,
    backoff: Duration,
    retry_at: Option<Instant>,
}

impl<S: PositionSink> WriteBuffer<S> {
    fn push(&mut self, row: PositionRow) {
        if self.queue.len() >= self.cfg.capacity {
            self.queue.pop_front();
            let dropped = self.stats.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped.is_power_of_two() {
                tracing::warn!(dropped, "write buffer full, dropping oldest positions");
            }
        }
        self.queue.push_back(row);
        self.stats.queued.store(self.queue.len(), Ordering::Relaxed);
    }

    /// Writes batches until the queue is empty or a write fails.
    async fn flush(&mut self) {
        if self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        while !self.queue.is_empty() {
            let n = self.queue.len().min(self.cfg.batch_size);
            let batch = &self.queue.make_contiguous()[..n];
            match self.sink.insert(batch).await {
                Ok(()) => {
                    self.queue.drain(..n);
                    self.stats.written.fetch_add(n as u64, Ordering::Relaxed);
                    if self.retry_at.take().is_some() {
                        tracing::info!("database writes recovered, replaying buffer");
                    }
                    self.backoff = self.cfg.initial_backoff;
                }
                Err(e) => {
                    self.stats.failed_flushes.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        error = %e,
                        queued = self.queue.len(),
                        retry_in_ms = u64::try_from(self.backoff.as_millis()).unwrap_or(u64::MAX),
                        "position insert failed, keeping rows buffered"
                    );
                    self.retry_at = Some(Instant::now() + self.backoff);
                    self.backoff = (self.backoff * 2).min(self.cfg.max_backoff);
                    break;
                }
            }
        }
        self.stats.queued.store(self.queue.len(), Ordering::Relaxed);
    }
}

/// Runs the buffer until `cancel` fires (or all senders are dropped), then
/// performs a final flush bounded by `cfg.shutdown_deadline`.
pub async fn run_writer<S: PositionSink>(
    sink: S,
    cfg: BufferConfig,
    mut rx: mpsc::Receiver<PositionRow>,
    stats: Arc<BufferStats>,
    cancel: CancellationToken,
) {
    let mut buf = WriteBuffer {
        sink,
        cfg,
        queue: VecDeque::new(),
        stats,
        backoff: cfg.initial_backoff,
        retry_at: None,
    };
    let mut tick = tokio::time::interval(cfg.flush_interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            () = cancel.cancelled() => break,
            row = rx.recv() => match row {
                Some(row) => {
                    buf.push(row);
                    if buf.queue.len() >= cfg.batch_size {
                        buf.flush().await;
                    }
                }
                None => break,
            },
            _ = tick.tick() => buf.flush().await,
        }
    }
    // Take what is still in the channel, then flush with a deadline.
    rx.close();
    while let Ok(row) = rx.try_recv() {
        buf.push(row);
    }
    buf.retry_at = None;
    let pending = buf.queue.len();
    let result = tokio::time::timeout(cfg.shutdown_deadline, async {
        while !buf.queue.is_empty() {
            buf.flush().await;
            if let Some(at) = buf.retry_at.take() {
                tokio::time::sleep_until(at).await;
            }
        }
    })
    .await;
    if result.is_ok() {
        tracing::info!(flushed = pending, "write buffer flushed");
    } else {
        tracing::error!(
            lost = buf.queue.len(),
            "shutdown deadline reached, unflushed positions lost"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::AtomicBool;

    use chrono::{TimeZone, Utc};
    use geo_core::Position;

    use super::*;

    /// Records inserted rows; fails while `down` is set.
    #[derive(Clone, Default)]
    struct FakeDb {
        rows: Arc<Mutex<Vec<u32>>>,
        down: Arc<AtomicBool>,
    }

    impl PositionSink for FakeDb {
        #[allow(clippy::unused_async_trait_impl)]
        async fn insert(&self, rows: &[PositionRow]) -> Result<(), String> {
            if self.down.load(Ordering::SeqCst) {
                return Err("connection refused".into());
            }
            self.rows
                .lock()
                .unwrap()
                .extend(rows.iter().map(|r| r.time_boot_ms));
            Ok(())
        }
    }

    fn row(i: u32) -> PositionRow {
        PositionRow {
            system_id: 1,
            time: Utc.timestamp_opt(1_700_000_000 + i64::from(i), 0).unwrap(),
            time_boot_ms: i,
            position: Position::new(48.0, 11.0, 500.0).unwrap(),
            relative_alt_m: 0.0,
            velocity_ned_mps: [0.0; 3],
            heading_deg: None,
        }
    }

    fn cfg(capacity: usize) -> BufferConfig {
        BufferConfig {
            capacity,
            batch_size: 4,
            flush_interval: Duration::from_millis(50),
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(400),
            shutdown_deadline: Duration::from_secs(2),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn replays_in_order_after_outage() {
        let db = FakeDb::default();
        db.down.store(true, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(64);
        let stats = Arc::new(BufferStats::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_writer(
            db.clone(),
            cfg(100),
            rx,
            stats.clone(),
            cancel.clone(),
        ));

        for i in 0..10 {
            tx.send(row(i)).await.unwrap();
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(db.rows.lock().unwrap().is_empty());
        assert_eq!(stats.snapshot().queued, 10);
        assert!(stats.snapshot().failed_flushes >= 2, "retried with backoff");

        db.down.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*db.rows.lock().unwrap(), (0..10).collect::<Vec<_>>());
        let s = stats.snapshot();
        assert_eq!((s.queued, s.written, s.dropped), (0, 10, 0));

        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn drops_oldest_when_full() {
        let db = FakeDb::default();
        db.down.store(true, Ordering::SeqCst);
        let (tx, rx) = mpsc::channel(64);
        let stats = Arc::new(BufferStats::default());
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run_writer(
            db.clone(),
            cfg(5),
            rx,
            stats.clone(),
            cancel.clone(),
        ));
        for i in 0..8 {
            tx.send(row(i)).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        db.down.store(false, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(*db.rows.lock().unwrap(), vec![3, 4, 5, 6, 7]);
        assert_eq!(stats.snapshot().dropped, 3);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn final_flush_on_shutdown() {
        let db = FakeDb::default();
        let (tx, rx) = mpsc::channel(64);
        let stats = Arc::new(BufferStats::default());
        let cancel = CancellationToken::new();
        let mut c = cfg(100);
        c.flush_interval = Duration::from_secs(3600); // never flush by timer
        let task = tokio::spawn(run_writer(db.clone(), c, rx, stats, cancel.clone()));
        for i in 0..3 {
            tx.send(row(i)).await.unwrap();
        }
        tokio::task::yield_now().await;
        cancel.cancel();
        task.await.unwrap();
        assert_eq!(*db.rows.lock().unwrap(), vec![0, 1, 2]);
    }
}
