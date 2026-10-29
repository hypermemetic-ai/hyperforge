//! `WorkspaceRunner` — reusable concurrency abstraction for workspace-level batch operations.
//!
//! Eliminates duplicated JoinSet/chunking boilerplate across workspace methods.
//! All functions return `Vec<(usize, R)>` where usize is the original index,
//! so callers can correlate results with inputs if needed.

use std::future::Future;
use tokio::task::JoinSet;

/// Split a Vec into chunks without requiring Clone.
fn chunk_vec<T>(items: Vec<T>, chunk_size: usize) -> Vec<Vec<T>> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    for item in items {
        current.push(item);
        if current.len() == chunk_size {
            result.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

/// Run a batch of blocking operations with bounded concurrency.
///
/// Each item is dispatched via `tokio::task::spawn_blocking`. Items are
/// processed in chunks of `concurrency` to bound resource usage. Results
/// are returned in completion order within each chunk.
///
/// Use this for git CLI operations and other synchronous work.
pub async fn run_batch_blocking<T, R, F>(
    items: Vec<T>,
    concurrency: usize,
    op: F,
) -> Vec<Result<R, String>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> R + Send + Sync + Clone + 'static,
{
    let len = items.len();
    let mut results = Vec::with_capacity(len);
    let chunk_size = if concurrency == 0 { len.max(1) } else { concurrency };

    let chunked = chunk_vec(items, chunk_size);

    for chunk in chunked {
        let mut join_set = JoinSet::new();

        for item in chunk {
            let op = op.clone();
            join_set.spawn(tokio::task::spawn_blocking(move || op(item)));
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(value)) => results.push(Ok(value)),
                Ok(Err(e)) => results.push(Err(format!("spawn_blocking panic: {e}"))),
                Err(e) => results.push(Err(format!("JoinSet error: {e}"))),
            }
        }
    }

    results
}

/// Run a batch of async operations with bounded concurrency.
///
/// Items are processed in chunks of `concurrency`. Use `concurrency = 0`
/// for unbounded (all items spawned at once).
///
/// Use this for forge API calls and other async work.
pub async fn run_batch<T, R, F, Fut>(
    items: Vec<T>,
    concurrency: usize,
    op: F,
) -> Vec<Result<R, String>>
where
    T: Send + 'static,
    R: Send + 'static,
    F: Fn(T) -> Fut + Send + Sync + Clone + 'static,
    Fut: Future<Output = R> + Send + 'static,
{
    let len = items.len();
    let mut results = Vec::with_capacity(len);
    let chunk_size = if concurrency == 0 { len.max(1) } else { concurrency };

    for chunk in chunk_vec(items, chunk_size) {
        let mut join_set = JoinSet::new();

        for item in chunk {
            let op = op.clone();
            join_set.spawn(async move { op(item).await });
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(value) => results.push(Ok(value)),
                Err(e) => results.push(Err(format!("JoinSet error: {e}"))),
            }
        }
    }

    results
}

/// Discover workspace or return an error event.
///
/// Replaces the 11+ instances of:
/// ```ignore
/// let ctx = match discover_workspace(&path) {
///     Ok(ctx) => ctx,
///     Err(e) => { yield HyperforgeEvent::Error { ... }; return; }
/// };
/// ```
// `HyperforgeEvent` is intentionally a broad enum carrying every event variant
// so callers can propagate failures uniformly as yield values. Boxing it here
// would force every call site through an extra dereference for no benefit.
#[allow(clippy::result_large_err)]
pub fn discover_or_bail(
    path: &std::path::Path,
) -> Result<crate::commands::workspace::WorkspaceContext, crate::service::HyperforgeEvent> {
    crate::commands::workspace::discover_workspace(path).map_err(|e| {
        crate::service::HyperforgeEvent::Error {
            message: format!("Discovery failed: {e}"),
        }
    })
}

/// Result of a parallel diff batch across org/forge pairs.
pub struct DiffBatchEntry {
    pub org_name: String,
    pub forge_name: String,
    pub diff_result: Result<crate::services::SyncDiff, String>,
}

/// Run diffs in parallel for a set of org/forge pairs.
///
/// This is the shared batch diff used by both the standalone `diff` method
/// and sync Phase 6. Callers iterate the results and yield events as needed.
pub async fn run_diff_batch(
    pairs: &[(String, String)],
    state: &crate::services::HyperforgeState,
    sync_service: &std::sync::Arc<crate::services::SymmetricSyncService>,
) -> Vec<Result<DiffBatchEntry, String>> {
    let items: Vec<(String, String, crate::services::HyperforgeState, std::sync::Arc<crate::services::SymmetricSyncService>)> = pairs
        .iter()
        .map(|(o, f)| (o.clone(), f.clone(), state.clone(), sync_service.clone()))
        .collect();
    run_batch(items, 8, |(org_name, forge_name, state, sync_service)| async move {
        let local = state.get_local_forge(&org_name).await;
        let ot = local.owner_type();
        let adapter = match crate::services::utils::make_adapter(&forge_name, &org_name, ot) {
            Ok(a) => a,
            Err(e) => {
                return DiffBatchEntry {
                    org_name,
                    forge_name,
                    diff_result: Err(e),
                };
            }
        };
        let result = sync_service
            .diff(local, adapter, &org_name)
            .await
            .map_err(|e| format!("Diff failed for {org_name}/{forge_name}: {e}"));
        DiffBatchEntry {
            org_name,
            forge_name,
            diff_result: result,
        }
    })
    .await
}

/// Result of processing push batch results into events.
pub struct PushBatchResult {
    /// Events to yield to the caller
    pub events: Vec<crate::service::HyperforgeEvent>,
    /// Number of repos where all pushes succeeded
    pub success_count: usize,
    /// Number of repos where at least one push failed
    pub failed_count: usize,
    /// Names of repos that failed
    pub failed_repos: Vec<String>,
}

/// Result row from a parallel push batch: `(dir_name, repo_path, push_outcome)`.
pub type PushBatchRow = (
    String,
    std::path::PathBuf,
    crate::commands::push::PushResult<crate::commands::push::PushReport>,
);

/// Process the results of a parallel push batch into events and counts.
///
/// This is the shared result processing used by both `push_all` and sync Phase 8.
pub fn collect_push_results(
    results: Vec<Result<PushBatchRow, String>>,
) -> PushBatchResult {
    let mut events = Vec::new();
    let mut success_count = 0usize;
    let mut failed_count = 0usize;
    let mut failed_repos = Vec::new();

    for result in results {
        let (dir_name, path, push_result) = match result {
            Ok(v) => v,
            Err(e) => {
                events.push(crate::service::HyperforgeEvent::Error {
                    message: format!("Task error: {e}"),
                });
                failed_count += 1;
                continue;
            }
        };

        match push_result {
            Ok(report) => {
                // Only emit events for failures
                for r in &report.results {
                    if !r.success {
                        events.push(crate::service::HyperforgeEvent::RepoPush {
                            repo_name: dir_name.clone(),
                            path: path.display().to_string(),
                            forge: r.forge.clone(),
                            success: false,
                            error: r.error.clone(),
                        });
                    }
                }
                if report.all_success {
                    success_count += 1;
                } else {
                    failed_count += 1;
                    failed_repos.push(dir_name.clone());
                }
            }
            Err(e) => {
                events.push(crate::service::HyperforgeEvent::RepoPush {
                    repo_name: dir_name.clone(),
                    path: path.display().to_string(),
                    forge: "all".to_string(),
                    success: false,
                    error: Some(e.to_string()),
                });
                failed_count += 1;
                failed_repos.push(dir_name.clone());
            }
        }
    }

    PushBatchResult {
        events,
        success_count,
        failed_count,
        failed_repos,
    }
}

/// Result of running a validation gate.
pub struct ValidationGateResult {
    /// Events to yield to the caller (`ValidateStep`, `ValidateSummary`, Error)
    pub events: Vec<crate::service::HyperforgeEvent>,
    /// Whether validation passed (None if not run, Some(true) if passed, Some(false) if failed)
    pub passed: Option<bool>,
}

/// Run the containerized validation gate.
///
/// Shared by `push_all` and sync's validation phase. Builds the dep graph,
/// creates a validation plan, executes it, and returns events + pass/fail status.
pub fn run_validation_gate(
    repos: &[crate::commands::workspace::DiscoveredRepo],
    workspace_root: &std::path::Path,
    is_dry_run: bool,
) -> ValidationGateResult {
    let graph = crate::commands::workspace::build_dep_graph(repos);
    let plan = crate::build_system::validate::build_validation_plan(&graph, &[], false);
    match plan {
        Ok(p) => {
            let results =
                crate::build_system::validate::execute_validation(&p, workspace_root, is_dry_run);
            let mut events = Vec::new();
            for r in &results {
                events.push(crate::service::HyperforgeEvent::ValidateStep {
                    repo_name: r.repo_name.clone(),
                    step: r.step.clone(),
                    status: format!("{}", r.status),
                    duration_ms: r.duration_ms,
                });
            }
            let summary = crate::build_system::validate::summarize_results(&results);
            let passed = summary.failed == 0;
            events.push(crate::service::HyperforgeEvent::ValidateSummary {
                total: summary.total,
                passed: summary.passed,
                failed: summary.failed,
                skipped: summary.skipped,
                duration_ms: summary.duration_ms,
            });
            if !passed {
                events.push(crate::service::HyperforgeEvent::Error {
                    message: format!(
                        "Validation failed ({}/{} steps failed) — aborting push.",
                        summary.failed, summary.total
                    ),
                });
            }
            ValidationGateResult {
                events,
                passed: Some(passed),
            }
        }
        Err(e) => ValidationGateResult {
            events: vec![crate::service::HyperforgeEvent::Error {
                message: format!("Validation plan failed: {e} — aborting push."),
            }],
            passed: Some(false),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_run_batch_blocking_basic() {
        let items: Vec<i32> = vec![1, 2, 3, 4, 5];
        let results = run_batch_blocking(items, 2, |x| x * 2).await;

        assert_eq!(results.len(), 5);
        let mut values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, vec![2, 4, 6, 8, 10]);
    }

    #[tokio::test]
    async fn test_run_batch_async_basic() {
        let items: Vec<i32> = vec![1, 2, 3, 4, 5];
        let results = run_batch(items, 2, |x| async move { x * 3 }).await;

        assert_eq!(results.len(), 5);
        let mut values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, vec![3, 6, 9, 12, 15]);
    }

    #[tokio::test]
    async fn test_run_batch_blocking_unbounded() {
        let items: Vec<i32> = (0..20).collect();
        let results = run_batch_blocking(items, 0, |x| x + 1).await;

        assert_eq!(results.len(), 20);
        let mut values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, (1..21).collect::<Vec<_>>());
    }

    #[tokio::test]
    async fn test_run_batch_empty() {
        let items: Vec<i32> = vec![];
        let results = run_batch_blocking(items, 4, |x| x).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_run_batch_async_unbounded() {
        let items: Vec<i32> = (0..10).collect();
        let results = run_batch(items, 0, |x| async move { x * x }).await;

        assert_eq!(results.len(), 10);
        let mut values: Vec<i32> = results.into_iter().map(|r| r.unwrap()).collect();
        values.sort_unstable();
        assert_eq!(values, vec![0, 1, 4, 9, 16, 25, 36, 49, 64, 81]);
    }
}
