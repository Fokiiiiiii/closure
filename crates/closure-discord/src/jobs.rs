//! Runs the jobs modules scheduled (see [`closure_core::schedule`]): looks for the ones that are
//! due and gives each to its module through the [`Dispatcher`].

use std::sync::Arc;
use std::time::Duration;

use closure_core::schedule::{ScheduledJob, SchedulerBackend, backoff};
use futures_util::future::join_all;
use tokio_util::sync::CancellationToken;

use crate::dispatcher::{Dispatcher, JobOutcome};

/// How often the store is asked for due jobs when the last ask found few.
const POLL: Duration = Duration::from_secs(1);
/// How many jobs are taken at once; a full batch is followed by another at once.
const BATCH: usize = 50;
/// How long a taken job is held for this runner. A job still not reported done after that is
/// handed out again (the bot probably stopped while it ran), so it has to be longer than any
/// handler may take, and than a job waits in a busy guild's queue.
const LEASE: Duration = Duration::from_secs(300);
/// How long a job that could not be run for no fault of its own waits.
const POSTPONE: Duration = Duration::from_secs(30);

/// Takes the jobs that are due and runs them until `stop` is cancelled.
pub async fn run_jobs(
    backend: Arc<dyn SchedulerBackend>,
    dispatcher: Dispatcher,
    stop: CancellationToken,
) {
    loop {
        let taken = tokio::select! {
            () = stop.cancelled() => return,
            taken = run_due(backend.as_ref(), &dispatcher) => taken,
        };
        if taken >= BATCH {
            continue;
        }
        tokio::select! {
            () = stop.cancelled() => return,
            () = tokio::time::sleep(POLL) => {}
        }
    }
}

/// Runs the jobs that are due now, together, and records how each went. How many were taken.
pub async fn run_due(backend: &dyn SchedulerBackend, dispatcher: &Dispatcher) -> usize {
    let jobs = match backend.claim_due(BATCH, LEASE).await {
        Ok(jobs) => jobs,
        Err(error) => {
            tracing::warn!(%error, "could not look for due jobs");
            return 0;
        }
    };
    let taken = jobs.len();
    join_all(
        jobs.into_iter()
            .map(|job| run_one(backend, dispatcher, job)),
    )
    .await;
    taken
}

async fn run_one(backend: &dyn SchedulerBackend, dispatcher: &Dispatcher, job: ScheduledJob) {
    let (id, token, attempt) = (job.id, job.token, job.attempt);
    let (module, name) = (job.module.clone(), job.name.clone());
    let outcome = dispatcher.run_job(job).await;
    metrics::counter!("closure_jobs_total", "module" => module.to_string(), "result" => match &outcome {
        JobOutcome::Done => "done",
        JobOutcome::Failed(_) => "failed",
        JobOutcome::Postponed => "postponed",
        JobOutcome::Gone => "gone",
        JobOutcome::Dropped => "dropped",
    })
    .increment(1);
    let recorded = match outcome {
        JobOutcome::Done => backend.complete(id, token).await,
        JobOutcome::Failed(error) => {
            tracing::warn!(%module, job = %name, attempt, %error, "a scheduled job failed");
            backend.retry(id, token, backoff(attempt), &error).await
        }
        JobOutcome::Postponed => backend.postpone(id, token, POSTPONE).await,
        JobOutcome::Gone => {
            tracing::warn!(%module, job = %name, "a scheduled job belongs to a module that is gone; it is dropped");
            backend.discard(id, token).await
        }
        JobOutcome::Dropped => {
            tracing::warn!(%module, job = %name, "the module cannot handle a scheduled job; it is dropped");
            backend.discard(id, token).await
        }
    };
    if let Err(error) = recorded {
        // The lease runs out and the job comes again.
        tracing::warn!(%module, job = %name, %error, "could not record how a scheduled job went");
    }
}
