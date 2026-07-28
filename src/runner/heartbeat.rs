use std::future::Future;
use std::time::Duration;

const MIN_RENEWAL_WINDOW_MS: u64 = 250;

pub(crate) enum HeartbeatOutcome<T, E> {
    Completed { output: T, renewals: u64 },
    LeaseLost { error: E, renewals: u64 },
}

pub(crate) fn renewal_delay(ttl_ms: u64, safety_margin_ms: u64) -> Option<Duration> {
    let delay_ms = ttl_ms.checked_sub(safety_margin_ms)?;
    (delay_ms >= MIN_RENEWAL_WINDOW_MS).then(|| Duration::from_millis(delay_ms))
}

pub(crate) async fn run_with_heartbeat<F, T, E, R, RF>(
    operation: F,
    ttl_ms: u64,
    safety_margin_ms: u64,
    mut renew: R,
) -> HeartbeatOutcome<T, E>
where
    F: Future<Output = T>,
    R: FnMut() -> RF,
    RF: Future<Output = Result<(), E>>,
{
    let Some(delay) = renewal_delay(ttl_ms, safety_margin_ms) else {
        return match renew().await {
            Ok(()) => HeartbeatOutcome::Completed {
                output: operation.await,
                renewals: 1,
            },
            Err(error) => HeartbeatOutcome::LeaseLost { error, renewals: 0 },
        };
    };

    tokio::pin!(operation);
    let mut renewals = 0u64;
    loop {
        tokio::select! {
            output = &mut operation => {
                return HeartbeatOutcome::Completed { output, renewals };
            }
            _ = tokio::time::sleep(delay) => {
                match renew().await {
                    Ok(()) => renewals = renewals.saturating_add(1),
                    Err(error) => {
                        return HeartbeatOutcome::LeaseLost { error, renewals };
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    use super::*;

    #[test]
    fn renewal_delay_preserves_the_configured_margin() {
        assert_eq!(
            renewal_delay(30_000, 15_000),
            Some(Duration::from_millis(15_000))
        );
        assert_eq!(renewal_delay(15_000, 15_000), None);
        assert_eq!(renewal_delay(100, 50), None);
    }

    #[tokio::test]
    async fn long_operation_is_renewed_until_completion() {
        let renewals = Arc::new(AtomicU64::new(0));
        let counter = renewals.clone();
        let outcome = run_with_heartbeat(
            async {
                tokio::time::sleep(Duration::from_millis(85)).await;
                "done"
            },
            50,
            20,
            move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Ok::<_, ()>(())
                }
            },
        )
        .await;
        match outcome {
            HeartbeatOutcome::Completed { output, renewals } => {
                assert_eq!(output, "done");
                assert!(renewals >= 2);
            }
            HeartbeatOutcome::LeaseLost { .. } => panic!("lease unexpectedly lost"),
        }
        assert!(renewals.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test]
    async fn renewal_failure_cancels_the_operation() {
        let outcome = run_with_heartbeat(
            std::future::pending::<()>(),
            50,
            20,
            || async { Err::<(), _>("stale fencing token") },
        )
        .await;
        match outcome {
            HeartbeatOutcome::LeaseLost { error, renewals } => {
                assert_eq!(error, "stale fencing token");
                assert_eq!(renewals, 0);
            }
            HeartbeatOutcome::Completed { .. } => panic!("pending operation completed"),
        }
    }
}
