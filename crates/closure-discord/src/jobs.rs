//! Runs the jobs modules scheduled (see [`closure_core::schedule`]): looks for the ones that are
//! due and gives each to its module through the [`Dispatcher`].

use std::sync::Arc;
use std::time::Duration;

use closure_core::schedule::{ScheduledJob, SchedulerBackend, backoff};
use futures_util::future::join_all;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
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

/// Takes the jobs that are due and runs them until `stop` is cancelled. A job runs as a task of its
/// own, so a slow one does not hold the next ones back: at most [`BATCH`] run at once, and a job is
/// claimed only when there is a place for it, so its lease does not run while it waits. When `stop`
/// is cancelled the jobs in flight are finished and reported first.
pub async fn run_jobs(
    backend: Arc<dyn SchedulerBackend>,
    dispatcher: Dispatcher,
    stop: CancellationToken,
) {
    let places = Arc::new(Semaphore::new(BATCH));
    let mut running = JoinSet::new();
    loop {
        if stop.is_cancelled() {
            break;
        }
        while running.try_join_next().is_some() {}
        let free = places.available_permits();
        let taken = if free == 0 {
            0
        } else {
            match backend.claim_due(free, LEASE).await {
                Ok(jobs) => {
                    let count = jobs.len();
                    for job in jobs {
                        let (backend, dispatcher) = (Arc::clone(&backend), dispatcher.clone());
                        let Ok(place) = Arc::clone(&places).acquire_owned().await else {
                            break;
                        };
                        running.spawn(async move {
                            run_one(backend.as_ref(), &dispatcher, job).await;
                            drop(place);
                        });
                    }
                    count
                }
                Err(error) => {
                    tracing::warn!(%error, "could not look for due jobs");
                    0
                }
            }
        };
        if free > 0 && taken >= free {
            continue;
        }
        tokio::select! {
            () = stop.cancelled() => break,
            () = tokio::time::sleep(POLL) => {}
        }
    }
    while running.join_next().await.is_some() {}
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
    // While the job runs, its lease is kept up: one that waits long in a busy guild's queue, or
    // runs long, is not handed out again, and a removal that cannot reach it is told so.
    let outcome = {
        let run = dispatcher.run_job(job);
        tokio::pin!(run);
        let mut beat = tokio::time::interval(LEASE / 3);
        beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        beat.tick().await;
        loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                _ = beat.tick() => match backend.renew(id, token, LEASE).await {
                    Ok(true) => {}
                    Ok(false) => tracing::warn!(%module, job = %name, "a running job is no longer the one held"),
                    Err(error) => tracing::warn!(%module, job = %name, %error, "could not keep the lease of a job"),
                },
            }
        }
    };
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
