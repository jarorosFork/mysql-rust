//! A dedicated writer thread for the on-disk log, giving concurrent writers
//! **group commit** (PERFORMANCE_DURABILITY_PLAN.md PD-2, fixing P3/P4).
//!
//! Before this module, every writer appended to the log directly, inline,
//! on whichever tokio worker thread happened to be running its statement —
//! meaning `fsync` (a multi-millisecond blocking syscall once
//! `SyncPolicy::Always` is on, see D1) ran *on the async runtime itself*,
//! stalling that worker's ability to poll any other connection, and 200
//! concurrent commits paid 200 separate fsyncs serialized behind one mutex.
//!
//! [`LogWriter`] fixes both at once with one design: it owns the [`Log`]
//! (and its `File`) on its own plain OS thread, fed by a bounded
//! `tokio::sync::mpsc` channel of already-framed records. Callers `.await`
//! a `tokio::sync::oneshot` ack that resolves once the record is durable —
//! never blocking a runtime worker, since the actual wait happens on a
//! genuine async channel, not a blocking call inline in the statement path.
//! The writer thread drains *everything* currently queued before writing —
//! not just the record that woke it — so N concurrent appends that land in
//! the channel while it's mid-write become one buffer, one `write_all`, and
//! (per `SyncPolicy`) one `fsync`, acking all N callers together. Group
//! commit falls out of "one dedicated owner thread" for free; it would not
//! from a pool of ad-hoc `spawn_blocking` threads, which is exactly why
//! this is a single long-lived thread rather than one-thread-per-write.

#[cfg(test)]
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

use crate::storage::log::{encode_create_table, encode_insert_row, encode_transaction, Log};
use crate::storage::value::{ColumnSchema, Value};
use crate::{Error, Result};

/// Generous headroom so `submit`'s channel send essentially never actually
/// waits under normal load — the bound exists for backpressure under
/// sustained overload (so a runaway producer can't grow this queue
/// unboundedly), not to throttle ordinary traffic.
const WRITER_QUEUE_CAPACITY: usize = 4096;

struct WriteRequest {
    framed: Vec<u8>,
    ack: oneshot::Sender<Result<()>>,
}

/// Owns an open [`Log`] on a dedicated thread; every append goes through
/// the bounded channel and is acked once it (and whatever else the writer
/// thread happened to batch alongside it) is durably written.
pub struct LogWriter {
    /// `None` only during/after `Drop` — see the comment there for why the
    /// sender has to be dropped before the thread is joined.
    sender: Option<mpsc::Sender<WriteRequest>>,
    join_handle: Option<std::thread::JoinHandle<()>>,
    /// Test-only: counts how many *physical* `write_framed_batch` calls the
    /// writer thread actually made, so a test can prove real batching
    /// happened (many logical appends, fewer physical writes) rather than
    /// just "it compiles and doesn't lose data" — mirrors `Log`'s own
    /// `sync_calls` seam.
    #[cfg(test)]
    batch_count: Arc<std::sync::atomic::AtomicUsize>,
    /// Test-only fault injection: while set, every batch the writer thread
    /// drains fails without touching the real log — same rationale as
    /// `InMemoryStorage::fail_next_log_write` (a genuine OS-level write
    /// failure isn't reliably triggerable on an already-open file handle).
    /// Deliberately *not* one-shot: several concurrent appends queued
    /// together can still land across more than one physical drain, and a
    /// one-shot flag would let a later drain in the same test silently
    /// succeed instead of proving every waiter observes the failure — see
    /// `set_fail_batches`.
    #[cfg(test)]
    fail_next_batch: Arc<std::sync::atomic::AtomicBool>,
}

impl LogWriter {
    /// Spawn the dedicated writer thread, which takes ownership of `log`
    /// (already open and fully replayed — see [`Log::open`]) for the rest
    /// of its life.
    pub fn spawn(log: Log) -> Self {
        let (sender, receiver) = mpsc::channel::<WriteRequest>(WRITER_QUEUE_CAPACITY);
        #[cfg(test)]
        let batch_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        #[cfg(test)]
        let batch_count_for_thread = Arc::clone(&batch_count);
        #[cfg(test)]
        let fail_next_batch = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(test)]
        let fail_next_batch_for_thread = Arc::clone(&fail_next_batch);

        let join_handle = std::thread::Builder::new()
            .name("mysql-rust-log-writer".to_string())
            .spawn(move || {
                Self::run(
                    log,
                    receiver,
                    #[cfg(test)]
                    batch_count_for_thread,
                    #[cfg(test)]
                    fail_next_batch_for_thread,
                )
            })
            .expect("failed to spawn the log-writer thread");

        LogWriter {
            sender: Some(sender),
            join_handle: Some(join_handle),
            #[cfg(test)]
            batch_count,
            #[cfg(test)]
            fail_next_batch,
        }
    }

    /// The writer thread's whole life: block for the first queued record,
    /// then drain whatever else has *already* accumulated (non-blocking) so
    /// concurrent appends batch into one write, then write once and ack
    /// everyone in the batch with the same outcome. Repeats until every
    /// [`mpsc::Sender`] (i.e. the owning `LogWriter`) is dropped.
    fn run(
        mut log: Log,
        mut receiver: mpsc::Receiver<WriteRequest>,
        #[cfg(test)] batch_count: Arc<std::sync::atomic::AtomicUsize>,
        #[cfg(test)] fail_next_batch: Arc<std::sync::atomic::AtomicBool>,
    ) {
        // `blocking_recv` is the tokio-sanctioned way to receive on a plain
        // (non-async) thread fed by async senders — unlike `.await`, this
        // one is *meant* to block, since this thread has no runtime of its
        // own to yield back to.
        while let Some(first) = receiver.blocking_recv() {
            let mut batch = vec![first];
            while let Ok(next) = receiver.try_recv() {
                batch.push(next);
            }

            let mut buf = Vec::with_capacity(batch.iter().map(|r| r.framed.len()).sum());
            for request in &batch {
                buf.extend_from_slice(&request.framed);
            }
            #[cfg(test)]
            let result = if fail_next_batch.load(std::sync::atomic::Ordering::SeqCst) {
                Err(Error::Io(std::io::Error::other(
                    "fault-injected log writer failure (test only)",
                )))
            } else {
                log.write_framed_batch(&buf)
            };
            #[cfg(not(test))]
            let result = log.write_framed_batch(&buf);
            #[cfg(test)]
            batch_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            match result {
                Ok(()) => {
                    for request in batch {
                        let _ = request.ack.send(Ok(()));
                    }
                }
                Err(e) => {
                    // `Error` isn't `Clone` (it wraps `std::io::Error`,
                    // which isn't either), so every waiter in a failed
                    // batch gets its own fresh `Error::Io` built from the
                    // same message rather than sharing one value.
                    let message = e.to_string();
                    for request in batch {
                        let _ = request
                            .ack
                            .send(Err(Error::Io(std::io::Error::other(message.clone()))));
                    }
                }
            }
        }
        // Every sender is gone (the `LogWriter` was dropped): exit,
        // dropping `log` — and its `File` — on this thread.
    }

    async fn submit(&self, framed: Vec<u8>) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        // `sender` is only ever `None` once `self` is being dropped, at
        // which point nothing can still be calling `submit` — see `Drop`.
        let sender = self
            .sender
            .as_ref()
            .expect("LogWriter::submit called after shutdown");
        sender
            .send(WriteRequest {
                framed,
                ack: ack_tx,
            })
            .await
            .map_err(|_| Error::Execution("log writer thread is not running".to_string()))?;
        ack_rx.await.map_err(|_| {
            Error::Execution("log writer thread stopped without acking a pending write".to_string())
        })?
    }

    pub async fn append_create_table(
        &self,
        table: &str,
        columns: &[ColumnSchema],
        primary_key: Option<&str>,
    ) -> Result<()> {
        self.submit(crate::storage::log::frame_record(&encode_create_table(
            table,
            columns,
            primary_key,
        )))
        .await
    }

    pub async fn append_insert_row(&self, table: &str, row: &[Value]) -> Result<()> {
        self.submit(crate::storage::log::frame_record(&encode_insert_row(
            table, row,
        )))
        .await
    }

    pub async fn append_transaction(&self, rows: &[(String, Vec<Value>)]) -> Result<()> {
        self.submit(crate::storage::log::frame_record(&encode_transaction(rows)))
            .await
    }

    /// Test-only: how many physical batched writes the thread has made so
    /// far — see the field doc comment.
    #[cfg(test)]
    fn batch_count(&self) -> usize {
        self.batch_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Test-only: make every batch the writer thread drains fail (`true`)
    /// or stop faulting and resume real writes (`false`) — see the field
    /// doc comment for why this isn't one-shot.
    #[cfg(test)]
    fn set_fail_batches(&self, fail: bool) {
        self.fail_next_batch
            .store(fail, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Drop for LogWriter {
    fn drop(&mut self) {
        // Drop the sender *before* joining. A struct's own `Drop::drop` body
        // runs before any of its fields are auto-dropped, so at this point
        // `self.sender` is still alive; the writer thread's `blocking_recv`
        // loop only exits once every sender is gone. Joining first (or
        // relying on the field's own drop, which happens *after* this
        // method returns) would wait forever on a thread that is itself
        // waiting for this drop to release the sender — take it explicitly,
        // now, so the channel actually closes before we wait for the
        // thread to notice.
        self.sender = None;
        if let Some(handle) = self.join_handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SyncPolicy;
    use crate::storage::value::ColumnType;

    fn temp_path(name: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
        let mut counter = COUNTER.lock().unwrap_or_else(|e| e.into_inner());
        *counter += 1;
        std::env::temp_dir().join(format!(
            "mysql-rust-log-writer-test-{name}-{}-{}",
            std::process::id(),
            *counter
        ))
    }

    fn int_col(name: &str) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            column_type: ColumnType::Int,
            nullable: false,
            auto_increment: false,
        }
    }

    #[tokio::test]
    async fn a_single_append_is_acked_and_durable() {
        let path = temp_path("single");
        let log = Log::open(&path, SyncPolicy::Always, |_| {}).unwrap();
        let writer = LogWriter::spawn(log);

        writer
            .append_create_table("t", &[int_col("id")], Some("id"))
            .await
            .unwrap();
        writer
            .append_insert_row("t", &[Value::Int(1)])
            .await
            .unwrap();
        drop(writer);

        let mut replayed = 0;
        Log::open(&path, SyncPolicy::Never, |_| replayed += 1).unwrap();
        assert_eq!(replayed, 2);

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn concurrent_appends_all_land_and_batch_into_fewer_physical_writes() {
        let path = temp_path("concurrent");
        let log = Log::open(&path, SyncPolicy::Always, |_| {}).unwrap();
        let writer = Arc::new(LogWriter::spawn(log));
        writer
            .append_create_table("t", &[int_col("id")], Some("id"))
            .await
            .unwrap();

        const N: i64 = 200;
        let mut handles = Vec::with_capacity(N as usize);
        for i in 0..N {
            let writer = Arc::clone(&writer);
            handles.push(tokio::spawn(async move {
                writer.append_insert_row("t", &[Value::Int(i)]).await
            }));
        }
        for handle in handles {
            handle.await.unwrap().unwrap();
        }

        // Real batching: 200 concurrent appends plus the initial CREATE
        // TABLE must not have cost 201 separate physical writes -- some
        // genuinely landed in the same drained batch. This is a low,
        // deliberately-not-flaky bound (batching is a scheduling-dependent
        // fact, not a guaranteed exact count); the benchmark suite
        // (`concurrent_commits`) is where the *performance* claim is
        // actually measured.
        let physical_writes = writer.batch_count();
        assert!(
            physical_writes < 201,
            "expected fewer physical writes than logical appends (group commit), got \
             {physical_writes} writes for 201 appends"
        );

        let writer = Arc::try_unwrap(writer).unwrap_or_else(|_| panic!("writer still shared"));
        drop(writer);

        let mut rows = 0;
        Log::open(&path, SyncPolicy::Never, |_| rows += 1).unwrap();
        assert_eq!(rows, 1 + N as usize, "every acked append must be durable");

        std::fs::remove_file(&path).ok();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn a_failed_batch_acks_every_waiter_in_it_with_an_error() {
        let path = temp_path("fault-injection");
        let log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
        let writer = Arc::new(LogWriter::spawn(log));

        // Arm the fault (stays on until explicitly cleared -- see the field
        // doc comment for why one-shot isn't safe here), then fire several
        // appends as genuinely concurrent tasks (not sequential `.await`s,
        // which would never overlap on the writer thread at all): no
        // matter how the writer thread splits them across physical drains
        // while the fault is armed, every single one must come back `Err`.
        writer.set_fail_batches(true);
        let mut handles = Vec::new();
        for i in 0..5 {
            let writer = Arc::clone(&writer);
            handles.push(tokio::spawn(async move {
                writer.append_insert_row("t", &[Value::Int(i)]).await
            }));
        }
        for handle in handles {
            assert!(
                handle.await.unwrap().is_err(),
                "every request in the faulted batch must observe the failure, not just the first"
            );
        }

        // Clearing the fault must let the writer thread resume real writes
        // -- proving it survived the failed batch(es) rather than, say,
        // having wedged or poisoned its owned `Log`.
        writer.set_fail_batches(false);
        writer
            .append_insert_row("t", &[Value::Int(99)])
            .await
            .unwrap();

        let writer = Arc::try_unwrap(writer).unwrap_or_else(|_| panic!("writer still shared"));
        drop(writer);
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn dropping_the_writer_joins_the_thread_without_hanging() {
        let path = temp_path("drop-joins");
        let log = Log::open(&path, SyncPolicy::Never, |_| {}).unwrap();
        let writer = LogWriter::spawn(log);
        writer
            .append_insert_row("t", &[Value::Int(1)])
            .await
            .unwrap();
        drop(writer); // must return promptly, not deadlock.
        std::fs::remove_file(&path).ok();
    }
}
