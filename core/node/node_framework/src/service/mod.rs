use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use anyhow::Context;
use futures::{future::BoxFuture, FutureExt};
use tokio::{
    runtime::Runtime,
    sync::{watch, Barrier},
};
use zksync_utils::panic_extractor::try_extract_panic_message;

pub use self::{context::ServiceContext, stop_receiver::StopReceiver};
use crate::{
    precondition::Precondition,
    resource::{ResourceId, StoredResource},
    task::{OneshotTask, Task, UnconstrainedOneshotTask, UnconstrainedTask},
    wiring_layer::{WiringError, WiringLayer},
};

mod context;
mod stop_receiver;
#[cfg(test)]
mod tests;

// A reasonable amount of time for any task to finish the shutdown process
const TASK_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

/// "Manager" class for a set of tasks. Collects all the resources and tasks,
/// then runs tasks until completion.
///
/// Initialization flow:
/// - Service instance is created with access to the resource provider.
/// - Wiring layers are added to the service. At this step, tasks are not created yet.
/// - Once the `run` method is invoked, service
///   - invokes a `wire` method on each added wiring layer. If any of the layers fails,
///     the service will return an error. If no layers have added a task, the service will
///     also return an error.
///   - waits for any of the tasks to finish.
///   - sends stop signal to all the tasks.
///   - waits for the remaining tasks to finish.
///   - returns the result of the task that has finished.
pub struct ZkStackService {
    /// Cache of resources that have been requested at least by one task.
    resources: HashMap<ResourceId, Box<dyn StoredResource>>,
    /// List of wiring layers.
    layers: Vec<Box<dyn WiringLayer>>,
    /// Preconditions added to the service.
    preconditions: Vec<Box<dyn Precondition>>,
    /// Tasks added to the service.
    tasks: Vec<Box<dyn Task>>,
    /// Oneshot tasks added to the service.
    oneshot_tasks: Vec<Box<dyn OneshotTask>>,
    /// Unconstrained tasks added to the service.
    unconstrained_tasks: Vec<Box<dyn UnconstrainedTask>>,
    /// Unconstrained oneshot tasks added to the service.
    unconstrained_oneshot_tasks: Vec<Box<dyn UnconstrainedOneshotTask>>,

    /// Sender used to stop the tasks.
    stop_sender: watch::Sender<bool>,
    /// Tokio runtime used to spawn tasks.
    runtime: Runtime,
}

impl fmt::Debug for ZkStackService {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ZkStackService").finish_non_exhaustive()
    }
}

impl ZkStackService {
    pub fn new() -> anyhow::Result<Self> {
        if tokio::runtime::Handle::try_current().is_ok() {
            anyhow::bail!(
                "Detected a Tokio Runtime. ZkStackService manages its own runtime and does not support nested runtimes"
            );
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        let (stop_sender, _stop_receiver) = watch::channel(false);
        Ok(Self {
            resources: HashMap::default(),
            layers: Vec::new(),
            preconditions: Vec::new(),
            tasks: Vec::new(),
            oneshot_tasks: Vec::new(),
            unconstrained_tasks: Vec::new(),
            unconstrained_oneshot_tasks: Vec::new(),
            stop_sender,
            runtime,
        })
    }

    /// Adds a wiring layer.
    /// During the [`run`](ZkStackService::run) call the service will invoke
    /// `wire` method of every layer in the order they were added.
    pub fn add_layer<T: WiringLayer>(&mut self, layer: T) -> &mut Self {
        self.layers.push(Box::new(layer));
        self
    }

    /// Runs the system.
    pub fn run(mut self) -> anyhow::Result<()> {
        // Initialize tasks.
        let wiring_layers = std::mem::take(&mut self.layers);

        let mut errors: Vec<(String, WiringError)> = Vec::new();

        let runtime_handle = self.runtime.handle().clone();
        for layer in wiring_layers {
            let name = layer.layer_name().to_string();
            let task_result =
                runtime_handle.block_on(layer.wire(ServiceContext::new(&name, &mut self)));
            if let Err(err) = task_result {
                // We don't want to bail on the first error, since it'll provide worse DevEx:
                // People likely want to fix as much problems as they can in one go, rather than have
                // to fix them one by one.
                errors.push((name, err));
                continue;
            };
        }

        // Report all the errors we've met during the init.
        if !errors.is_empty() {
            for (layer, error) in errors {
                tracing::error!("Wiring layer {layer} can't be initialized: {error}");
            }
            anyhow::bail!("One or more wiring layers failed to initialize");
        }

        // We don't count preconditions as tasks.
        if self.tasks.is_empty()
            && self.unconstrained_tasks.is_empty()
            && self.oneshot_tasks.is_empty()
            && self.unconstrained_oneshot_tasks.is_empty()
        {
            anyhow::bail!("No tasks have been added to the service");
        }

        // Check whether we *only* run oneshot tasks.
        // If that's the case, then the support task that drives oneshot tasks will exit as soon as they are
        // completed.
        let only_oneshot_tasks = self.tasks.is_empty() && self.unconstrained_tasks.is_empty();

        // Barrier that will only be lifted once all the preconditions are met.
        // It will be awaited by the tasks before they start running and by the preconditions once they are fulfilled.
        let task_barrier = Arc::new(Barrier::new(
            self.tasks.len() + self.preconditions.len() + self.oneshot_tasks.len(),
        ));

        // Collect long-running tasks.
        let mut tasks: Vec<BoxFuture<'static, anyhow::Result<()>>> = Vec::new();
        self.collect_unconstrained_tasks(&mut tasks);
        self.collect_tasks(&mut tasks, task_barrier.clone());

        // Collect oneshot tasks (including preconditions).
        let mut oneshot_tasks: Vec<BoxFuture<'static, anyhow::Result<()>>> = Vec::new();
        self.collect_preconditions(&mut oneshot_tasks, task_barrier.clone());
        self.collect_oneshot_tasks(&mut oneshot_tasks, task_barrier.clone());
        self.collect_unconstrained_oneshot_tasks(&mut oneshot_tasks);

        // Wiring is now complete.
        for resource in self.resources.values_mut() {
            resource.stored_resource_wired();
        }
        tracing::info!("Wiring complete");

        // Create a system task that is cancellation-aware and will only exit on either precondition failure or
        // stop signal.
        let mut stop_receiver = self.stop_receiver();
        let precondition_system_task = Box::pin(async move {
            let oneshot_tasks = oneshot_tasks.into_iter().map(|fut| async move {
                // Spawn each oneshot task as a separate tokio task.
                // This way we can handle the cases when such a task panics and propagate the message
                // to the service.
                let handle = tokio::runtime::Handle::current();
                match handle.spawn(fut).await {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(err)) => Err(err),
                    Err(panic_err) => {
                        let panic_msg = try_extract_panic_message(panic_err);
                        Err(anyhow::format_err!("Precondition panicked: {panic_msg}"))
                    }
                }
            });

            match futures::future::try_join_all(oneshot_tasks).await {
                Err(err) => Err(err),
                Ok(_) if only_oneshot_tasks => {
                    // We only run oneshot tasks in this service, so we can exit now.
                    Ok(())
                }
                Ok(_) => {
                    // All oneshot tasks have exited and we have at least one long-running task.
                    // Simply wait for the stop signal.
                    stop_receiver.0.changed().await.ok();
                    Ok(())
                }
            }
            // Note that we don't have to `select` on the stop signal explicitly:
            // Each prerequisite is given a stop signal, and if everyone respects it, this future
            // will still resolve once the stop signal is received.
        });
        tasks.push(precondition_system_task);

        // Prepare tasks for running.
        let rt_handle = self.runtime.handle().clone();
        let join_handles: Vec<_> = tasks
            .into_iter()
            .map(|task| rt_handle.spawn(task).fuse())
            .collect();

        // Run the tasks until one of them exits.
        let (resolved, _, remaining) = self
            .runtime
            .block_on(futures::future::select_all(join_handles));
        let failure = match resolved {
            Ok(Ok(())) => false,
            Ok(Err(err)) => {
                tracing::error!("One of the tasks exited with an error: {err:?}");
                true
            }
            Err(panic_err) => {
                let panic_msg = try_extract_panic_message(panic_err);
                tracing::error!("One of the tasks panicked: {panic_msg}");
                true
            }
        };

        let remaining_tasks_with_timeout: Vec<_> = remaining
            .into_iter()
            .map(|task| async { tokio::time::timeout(TASK_SHUTDOWN_TIMEOUT, task).await })
            .collect();

        // Send stop signal to remaining tasks and wait for them to finish.
        // Given that we are shutting down, we do not really care about returned values.
        self.stop_sender.send(true).ok();
        let execution_results = self
            .runtime
            .block_on(futures::future::join_all(remaining_tasks_with_timeout));
        let execution_timeouts_count = execution_results.iter().filter(|&r| r.is_err()).count();
        if execution_timeouts_count > 0 {
            tracing::warn!(
                "{execution_timeouts_count} tasks didn't finish in {TASK_SHUTDOWN_TIMEOUT:?} and were dropped"
            );
        } else {
            tracing::info!("Remaining tasks finished without reaching timeouts");
        }

        if failure {
            anyhow::bail!("Task failed");
        } else {
            Ok(())
        }
    }

    pub(crate) fn stop_receiver(&self) -> StopReceiver {
        StopReceiver(self.stop_sender.subscribe())
    }

    fn collect_unconstrained_tasks(
        &mut self,
        tasks: &mut Vec<BoxFuture<'static, anyhow::Result<()>>>,
    ) {
        for task in std::mem::take(&mut self.unconstrained_tasks) {
            let name = task.name();
            let stop_receiver = self.stop_receiver();
            let task_future = Box::pin(async move {
                task.run_unconstrained(stop_receiver)
                    .await
                    .with_context(|| format!("Task {name} failed"))
            });
            tasks.push(task_future);
        }
    }

    fn collect_tasks(
        &mut self,
        tasks: &mut Vec<BoxFuture<'static, anyhow::Result<()>>>,
        task_barrier: Arc<Barrier>,
    ) {
        for task in std::mem::take(&mut self.tasks) {
            let name = task.name();
            let stop_receiver = self.stop_receiver();
            let task_barrier = task_barrier.clone();
            let task_future = Box::pin(async move {
                task.run_with_barrier(stop_receiver, task_barrier)
                    .await
                    .with_context(|| format!("Task {name} failed"))
            });
            tasks.push(task_future);
        }
    }

    fn collect_preconditions(
        &mut self,
        oneshot_tasks: &mut Vec<BoxFuture<'static, anyhow::Result<()>>>,
        task_barrier: Arc<Barrier>,
    ) {
        for precondition in std::mem::take(&mut self.preconditions) {
            let name = precondition.name();
            let stop_receiver = self.stop_receiver();
            let task_barrier = task_barrier.clone();
            let task_future = Box::pin(async move {
                precondition
                    .check_with_barrier(stop_receiver, task_barrier)
                    .await
                    .with_context(|| format!("Precondition {name} failed"))
            });
            oneshot_tasks.push(task_future);
        }
    }

    fn collect_oneshot_tasks(
        &mut self,
        oneshot_tasks: &mut Vec<BoxFuture<'static, anyhow::Result<()>>>,
        task_barrier: Arc<Barrier>,
    ) {
        for oneshot_task in std::mem::take(&mut self.oneshot_tasks) {
            let name = oneshot_task.name();
            let stop_receiver = self.stop_receiver();
            let task_barrier = task_barrier.clone();
            let task_future = Box::pin(async move {
                oneshot_task
                    .run_oneshot_with_barrier(stop_receiver, task_barrier)
                    .await
                    .with_context(|| format!("Oneshot task {name} failed"))
            });
            oneshot_tasks.push(task_future);
        }
    }

    fn collect_unconstrained_oneshot_tasks(
        &mut self,
        oneshot_tasks: &mut Vec<BoxFuture<'static, anyhow::Result<()>>>,
    ) {
        for unconstrained_oneshot_task in std::mem::take(&mut self.unconstrained_oneshot_tasks) {
            let name = unconstrained_oneshot_task.name();
            let stop_receiver = self.stop_receiver();
            let task_future = Box::pin(async move {
                unconstrained_oneshot_task
                    .run_unconstrained_oneshot(stop_receiver)
                    .await
                    .with_context(|| format!("Unconstrained oneshot task {name} failed"))
            });
            oneshot_tasks.push(task_future);
        }
    }
}
