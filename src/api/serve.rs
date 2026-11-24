//! Running the queen: the reconcile, janitor and host-lost loops beside the
//! HTTP server, and a shutdown bounded by [`SHUTDOWN_GRACE`].

use std::sync::Arc;

use time::OffsetDateTime;

use super::router;
use crate::queen::Queen;

/// How long shutdown waits for in-flight requests and passes before it
/// abandons them. systemd's stop timeout is far longer; a restart that waits
/// out a slow GitHub pass leaves every drone's heartbeat failing meanwhile.
pub const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the process waits, once its work has returned, for blocking
/// tasks it never awaits (a DNS lookup on a resolver thread, say) before it
/// exits without them.
pub const RUNTIME_SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(1);

/// Run `work` to completion on `runtime`, then shut the runtime down within
/// [`RUNTIME_SHUTDOWN`]. Dropping a runtime waits for every running blocking
/// task without a deadline, so a lookup stuck on an unreachable resolver
/// would hold the process open after [`serve`] has already returned.
pub fn block_on_bounded<F: std::future::Future>(
    runtime: tokio::runtime::Runtime,
    work: F,
) -> F::Output {
    let out = runtime.block_on(work);
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    out
}

/// Run the reconciliation loop and the HTTP server until `shutdown` resolves,
/// then exit within [`SHUTDOWN_GRACE`].
pub async fn serve(
    queen: Arc<Queen>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    serve_with_grace(queen, listener, shutdown, SHUTDOWN_GRACE).await
}

/// [`serve`] with an explicit shutdown grace.
///
/// When `shutdown` resolves the listener stops accepting and the loops stop
/// starting passes. Requests and passes still in flight get `grace` to finish;
/// whatever remains is then abandoned at its current await point. That is
/// safe for durable state: every store write is one SQLite statement or a
/// transaction that rolls back when dropped, and the GitHub effects a pass
/// can leave unrecorded converge on the next queen's passes (an unowned
/// offline runner is reaped, a cancel or rerun is re-read from GitHub).
pub async fn serve_with_grace(
    queen: Arc<Queen>,
    listener: tokio::net::TcpListener,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
    grace: std::time::Duration,
) -> std::io::Result<()> {
    let (stop_tx, mut stop_rx) = tokio::sync::watch::channel(false);
    let mut stopping = stop_rx.clone();
    let stop_rx_janitor = stop_rx.clone();
    let stop_rx_host_lost = stop_rx.clone();
    let ticker = {
        let queen = Arc::clone(&queen);
        tokio::spawn(async move {
            let mut every = tokio::time::interval(std::time::Duration::from_secs(
                queen.config().poll_interval_secs,
            ));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Both arms are cancel-safe: `interval.tick` and `changed` hold
                // no state that a dropped future could lose. The tick itself
                // runs in the arm body, outside the select, so shutdown waits
                // for an in-progress pass instead of abandoning a mint.
                tokio::select! {
                    _ = every.tick() => {
                        let now = OffsetDateTime::now_utc();
                        if let Err(e) = queen.tick(now).await {
                            tracing::warn!("tick failed: {e}");
                            queen.record_tick_error(now, &e);
                        }
                    }
                    _ = stop_rx.changed() => break,
                }
            }
        })
    };
    let host_lost_task = crate::hostlost::spawn_passes(Arc::clone(&queen), stop_rx_host_lost);
    let janitor_task = queen.janitor().map(|_| {
        let queen = Arc::clone(&queen);
        let mut stop_rx = stop_rx_janitor;
        tokio::spawn(async move {
            let Some(janitor) = queen.janitor() else {
                return;
            };
            let mut every = tokio::time::interval(std::time::Duration::from_secs(
                janitor.settings().interval_secs,
            ));
            every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                // Cancel-safe for the same reason as the ticker above.
                tokio::select! {
                    _ = every.tick() => {
                        janitor.pass(janitor.settings().pass_repos(&queen.config().repos)).await;
                        if let Some(requeuer) = queen.requeuer() {
                            let now = OffsetDateTime::now_utc().unix_timestamp();
                            if let Err(e) = requeuer.pass(queen.store(), now).await {
                                tracing::warn!("requeue pass failed: {e}");
                            }
                        }
                    }
                    _ = stop_rx.changed() => break,
                }
            }
        })
    });
    let served = axum::serve(
        listener,
        router(queen).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        let _ = stop_tx.send(true);
    });
    let mut tasks = vec![ticker];
    tasks.extend(janitor_task);
    tasks.extend(host_lost_task);
    let aborts: Vec<_> = tasks.iter().map(|t| t.abort_handle()).collect();
    let drain = async move {
        let served = served.await;
        for task in tasks {
            let _ = task.await;
        }
        served
    };
    let deadline = async move {
        if stopping.wait_for(|stop| *stop).await.is_err() {
            // The sender only drops after the signal was sent.
            return std::future::pending().await;
        }
        tracing::info!(?grace, "queen shutting down");
        tokio::time::sleep(grace).await;
    };
    tokio::select! {
        biased;
        served = drain => served,
        () = deadline => {
            tracing::warn!(
                ?grace,
                "shutdown grace elapsed; abandoning in-flight requests and passes"
            );
            for abort in aborts {
                abort.abort();
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_process_returns_after_its_work_while_a_blocking_task_is_stuck() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let started = std::time::Instant::now();
        let out = block_on_bounded(runtime, async {
            // Like a getaddrinfo call on an unreachable resolver: nothing awaits it.
            let (running, started) = tokio::sync::oneshot::channel();
            drop(tokio::task::spawn_blocking(move || {
                let _ = running.send(());
                std::thread::sleep(std::time::Duration::from_secs(60));
            }));
            started.await.unwrap();
            7
        });
        assert_eq!(out, 7);
        let took = started.elapsed();
        assert!(took < RUNTIME_SHUTDOWN * 3, "returned after {took:?}");
    }
}
