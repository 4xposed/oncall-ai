use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
}

/// # Errors
///
/// Returns the first error `keep_trying` declines to retry.
pub async fn with_backoff<T, E, Fut>(
    backoff: Backoff,
    mut keep_trying: impl FnMut(&E) -> bool,
    mut fun: impl FnMut() -> Fut,
) -> Result<T, E>
where
    E: std::fmt::Display,
    Fut: Future<Output = Result<T, E>>,
{
    let mut delay = backoff.initial;
    loop {
        match fun().await {
            Ok(value) => return Ok(value),
            Err(error) => {
                if !keep_trying(&error) {
                    return Err(error);
                }
                tracing::warn!(
                    %error,
                    delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
                    "attempt failed, backing off"
                );
                tokio::time::sleep(delay).await;
                delay = delay.saturating_mul(2).min(backoff.max);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::time::Duration;

    fn tiny() -> Backoff {
        Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(1),
        }
    }

    #[tokio::test]
    async fn first_success_needs_no_retry() {
        let calls = Cell::new(0u32);
        let result: Result<u32, String> = with_backoff(
            tiny(),
            |_| true,
            || async {
                calls.set(calls.get() + 1);
                Ok(7)
            },
        )
        .await;
        assert_eq!(result.expect("first call succeeds"), 7);
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn declined_error_returns_without_another_attempt() {
        let calls = Cell::new(0u32);
        let result: Result<u32, String> = with_backoff(
            tiny(),
            |_| false,
            || async {
                calls.set(calls.get() + 1);
                Err("boom".to_owned())
            },
        )
        .await;
        assert_eq!(result.expect_err("policy declines the first error"), "boom");
        assert_eq!(calls.get(), 1);
    }

    #[tokio::test]
    async fn stateful_policy_caps_attempts() {
        let calls = Cell::new(0u32);
        let mut seen = 0u32;
        let result: Result<u32, String> = with_backoff(
            tiny(),
            |_| {
                seen += 1;
                seen < 3
            },
            || async {
                calls.set(calls.get() + 1);
                Err(format!("attempt {}", calls.get()))
            },
        )
        .await;
        assert_eq!(
            result.expect_err("third error is returned"),
            "attempt 3",
            "the policy must see errors in call order"
        );
        assert_eq!(calls.get(), 3);
    }

    #[tokio::test]
    async fn backoff_delays_grow_up_to_the_cap() {
        let calls = Cell::new(0u32);
        let backoff = Backoff {
            initial: Duration::from_millis(1),
            max: Duration::from_millis(4),
        };
        let started = std::time::Instant::now();
        let result: Result<u32, String> = with_backoff(
            backoff,
            |_| true,
            || async {
                calls.set(calls.get() + 1);
                if calls.get() < 4 {
                    Err("not yet".to_owned())
                } else {
                    Ok(calls.get())
                }
            },
        )
        .await;
        assert_eq!(result.expect("fourth call succeeds"), 4);
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(7),
            "backoffs of 1+2+4 ms must elapse, took {elapsed:?}"
        );
    }
}
