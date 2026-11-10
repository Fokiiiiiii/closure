//! Two modules of one bot have a job under the same name: the job runs only in the module that
//! scheduled it, and the other module's runner hands it back without touching the caller's data.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Command, CommandCx, JobCx, ModuleBuilder, ModuleError, UserError};
use serde_json::json;

async fn start(cx: CommandCx, (): ()) -> Result<(), UserError> {
    cx.ctx()
        .scheduler()
        .job("tick", &"beta")
        .unwrap()
        .after(Duration::ZERO)
        .await
        .unwrap();
    Ok(())
}

async fn tick(cx: JobCx) -> Result<(), ModuleError> {
    let payload: String = cx.payload()?;
    cx.store()
        .set("ran", &payload)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if payload == "beta" {
        cx.scheduler()
            .job("tick", &"follow")
            .map_err(|error| ModuleError::other(error.to_string()))?
            .after(Duration::ZERO)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
    }
    Ok(())
}

fn alpha() -> ModuleBuilder {
    ModuleBuilder::new("alpha", "Alpha", "0.1.0")
        .unwrap()
        .capability(Capability::Schedule)
        .capability(Capability::Storage)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .job("tick", tick)
}

fn beta() -> ModuleBuilder {
    ModuleBuilder::new("beta", "Beta", "0.1.0")
        .unwrap()
        .capability(Capability::Schedule)
        .capability(Capability::Storage)
        .config(json!({ "type": "object", "properties": {} }), json!({}))
        .command(Command::<()>::new("start").handler(start))
        .job("tick", tick)
}

#[tokio::test]
async fn a_job_of_another_module_with_the_same_name_is_not_run_in_this_one() {
    let alpha = Harness::new(alpha()).unwrap();
    let beta = alpha.sibling(beta()).unwrap();
    beta.invoke(Invocation::new("start")).await.unwrap();

    let results = alpha.run_due_jobs().await;
    assert!(results.is_empty(), "{results:?}");
    assert_eq!(alpha.store().get::<String>("ran").await.unwrap(), None);

    let results = beta.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(
        beta.store().get::<String>("ran").await.unwrap().as_deref(),
        Some("beta")
    );

    let results = beta.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(
        beta.store().get::<String>("ran").await.unwrap().as_deref(),
        Some("follow"),
        "the follow-up job stays with beta"
    );
    assert_eq!(alpha.store().get::<String>("ran").await.unwrap(), None);
}
