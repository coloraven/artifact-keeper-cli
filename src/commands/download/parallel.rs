//! Bounded concurrency helpers for ferry downloads.

use std::sync::Arc;

use miette::Result;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use super::errors::UnitError;
use crate::error::AkError;

/// Default parallel worker count: CPU count, clamped to 1..=16.
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

pub fn resolve_jobs(explicit: Option<usize>) -> usize {
    explicit
        .filter(|&n| n > 0)
        .unwrap_or_else(default_jobs)
        .max(1)
}

/// Run blocking work with at most `jobs` in flight.
/// Hard errors (join / panic) abort; per-item `Err` values are returned in `errors`.
pub async fn run_blocking_jobs_soft<T, R, F>(
    jobs: usize,
    items: Vec<T>,
    f: F,
) -> Result<(Vec<R>, Vec<UnitError>)>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> std::result::Result<R, UnitError> + Send + Sync + 'static,
{
    if items.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let sem = Arc::new(Semaphore::new(jobs.max(1)));
    let f = Arc::new(f);
    let mut set = JoinSet::new();

    for item in items {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AkError::ConfigError(format!("job semaphore closed: {e}")))?;
        let f = Arc::clone(&f);
        set.spawn_blocking(move || {
            let _permit = permit;
            f(item)
        });
    }

    let mut ok = Vec::new();
    let mut errors = Vec::new();
    while let Some(joined) = set.join_next().await {
        let res = joined.map_err(|e| AkError::ConfigError(format!("worker join failed: {e}")))?;
        match res {
            Ok(v) => ok.push(v),
            Err(e) => errors.push(e),
        }
    }
    Ok((ok, errors))
}

/// Strict variant: any item error aborts the batch.
pub async fn run_blocking_jobs<T, R, F>(jobs: usize, items: Vec<T>, f: F) -> Result<Vec<R>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Result<R> + Send + Sync + 'static,
{
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let sem = Arc::new(Semaphore::new(jobs.max(1)));
    let f = Arc::new(f);
    let mut set = JoinSet::new();

    for item in items {
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .map_err(|e| AkError::ConfigError(format!("job semaphore closed: {e}")))?;
        let f = Arc::clone(&f);
        set.spawn_blocking(move || {
            let _permit = permit;
            f(item)
        });
    }

    let mut out = Vec::with_capacity(set.len());
    while let Some(joined) = set.join_next().await {
        let res = joined.map_err(|e| AkError::ConfigError(format!("worker join failed: {e}")))?;
        out.push(res?);
    }
    Ok(out)
}
