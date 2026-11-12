//! Application-owned jobs. Cancellation never detaches a blocking worker.
use crate::models::{OperationId, Scope};
use anyhow::{Result, ensure};
use std::{
    collections::BTreeMap,
    future::Future,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};
use tokio::{runtime::Handle, sync::watch, task::JoinHandle};

static NEXT: AtomicU64 = AtomicU64::new(0);
#[derive(Clone, Debug)]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Cancelled,
    Failed(String),
}

pub struct Operation {
    pub id: OperationId,
    pub scope: Scope,
    pub name: String,
    pub flag: Arc<AtomicBool>,
    cancel: watch::Sender<bool>,
    status: Mutex<Status>,
}
impl Operation {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.cancel.send_replace(true);
    }
    pub fn cancellation(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }
    pub fn running(&self) {
        *self.status.lock().unwrap() = Status::Running;
    }
    fn finish(&self, result: Result<()>) {
        *self.status.lock().unwrap() = match result {
            Ok(()) => Status::Succeeded,
            Err(_) if self.flag.load(Ordering::Acquire) => Status::Cancelled,
            Err(error) => Status::Failed(format!("{error:#}")),
        };
    }
}

enum Worker {
    Async(JoinHandle<()>),
    Blocking(thread::JoinHandle<()>),
}
pub struct Operations {
    handle: Handle,
    stopping: AtomicBool,
    jobs: Mutex<BTreeMap<OperationId, Arc<Operation>>>,
    workers: Mutex<Vec<Worker>>,
}
impl Operations {
    pub fn new(handle: Handle) -> Arc<Self> {
        Arc::new(Self {
            handle,
            stopping: AtomicBool::new(false),
            jobs: Mutex::new(BTreeMap::new()),
            workers: Mutex::new(vec![]),
        })
    }
    fn register(&self, scope: Scope, name: &str, flag: Arc<AtomicBool>) -> Result<Arc<Operation>> {
        ensure!(
            !self.stopping.load(Ordering::Acquire),
            "Application is shutting down"
        );
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_nanos();
        let id =
            OperationId::try_from(format!("{nanos:x}-{sequence:x}")).map_err(anyhow::Error::msg)?;
        let (cancel, _) = watch::channel(false);
        let mut scope = scope;
        scope.operation = Some(id.clone());
        Ok(Arc::new(Operation {
            id,
            scope,
            name: name.into(),
            flag,
            cancel,
            status: Mutex::new(Status::Queued),
        }))
    }
    pub fn spawn<F, Fut>(&self, scope: Scope, name: &str, work: F) -> Result<Arc<Operation>>
    where
        F: FnOnce(Arc<Operation>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        let mut jobs = self.jobs.lock().unwrap();
        self.reap(&mut jobs);
        let operation = self.register(scope, name, Arc::new(AtomicBool::new(false)))?;
        let job = operation.clone();
        job.running();
        let execution_job = job.clone();
        let execution = self.handle.spawn(async move { work(execution_job).await });
        let worker = self.handle.spawn(async move {
            let result = execution
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            job.finish(result);
        });
        self.workers.lock().unwrap().push(Worker::Async(worker));
        jobs.insert(operation.id.clone(), operation.clone());
        Ok(operation)
    }
    pub fn spawn_blocking(
        &self,
        scope: Scope,
        name: &str,
        flag: Arc<AtomicBool>,
        work: impl FnOnce(Arc<Operation>) -> Result<()> + Send + 'static,
    ) -> Result<Arc<Operation>> {
        let mut jobs = self.jobs.lock().unwrap();
        self.reap(&mut jobs);
        let operation = self.register(scope, name, flag)?;
        let job = operation.clone();
        let worker = thread::Builder::new().name(name.into()).spawn(move || {
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(job.clone())))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("Background worker panicked")));
            job.finish(result);
        })?;
        self.workers.lock().unwrap().push(Worker::Blocking(worker));
        jobs.insert(operation.id.clone(), operation.clone());
        Ok(operation)
    }
    fn reap(&self, jobs: &mut BTreeMap<OperationId, Arc<Operation>>) {
        self.workers.lock().unwrap().retain(|worker| match worker {
            Worker::Async(worker) => !worker.is_finished(),
            Worker::Blocking(worker) => !worker.is_finished(),
        });
        while jobs.len() >= 200 {
            let completed = jobs
                .iter()
                .find(|(_, job)| !matches!(job.status(), Status::Queued | Status::Running))
                .map(|(id, _)| id.clone());
            if let Some(id) = completed {
                jobs.remove(&id);
            } else {
                break;
            }
        }
    }
    pub fn snapshot(&self) -> Vec<Arc<Operation>> {
        self.jobs.lock().unwrap().values().cloned().collect()
    }
    pub fn stop(&self) {
        let jobs = self.jobs.lock().unwrap();
        self.stopping.store(true, Ordering::Release);
        for job in jobs.values() {
            if matches!(job.status(), Status::Queued | Status::Running) {
                job.cancel();
            }
        }
    }
    pub async fn join(&self) {
        self.stop();
        let workers = std::mem::take(&mut *self.workers.lock().unwrap());
        for worker in workers {
            match worker {
                Worker::Async(worker) => {
                    let _ = worker.await;
                }
                Worker::Blocking(worker) => {
                    let _ = tokio::task::spawn_blocking(move || worker.join()).await;
                }
            }
        }
    }
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

pub struct RestoreLimit {
    limit: AtomicUsize,
    active: Mutex<usize>,
    changed: Condvar,
}
pub struct RestorePermit(Arc<RestoreLimit>);
impl RestoreLimit {
    pub fn new(limit: usize) -> Arc<Self> {
        Arc::new(Self {
            limit: AtomicUsize::new(limit.max(1)),
            active: Mutex::new(0),
            changed: Condvar::new(),
        })
    }
    pub fn configure(&self, limit: usize) {
        self.limit.store(limit.max(1), Ordering::Release);
        self.changed.notify_all();
    }
    pub fn acquire(self: &Arc<Self>, cancelled: &AtomicBool) -> Result<RestorePermit> {
        let mut active = self.active.lock().unwrap();
        loop {
            ensure!(
                !cancelled.load(Ordering::Acquire),
                "Operation cancelled while queued"
            );
            if *active < self.limit.load(Ordering::Acquire) {
                *active += 1;
                return Ok(RestorePermit(self.clone()));
            }
            active = self
                .changed
                .wait_timeout(active, Duration::from_millis(100))
                .unwrap()
                .0;
        }
    }
}
impl Drop for RestorePermit {
    fn drop(&mut self) {
        *self.0.active.lock().unwrap() -= 1;
        self.0.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn shutdown_cancels_and_joins_a_blocking_worker() -> Result<()> {
        let jobs = Operations::new(Handle::current());
        let (send, receive) = std::sync::mpsc::channel();
        let job = jobs.spawn_blocking(
            Scope::default(),
            "test",
            Arc::new(AtomicBool::new(false)),
            move |job| {
                while !job.flag.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(2));
                }
                send.send(())?;
                anyhow::bail!("Cancelled")
            },
        )?;
        jobs.join().await;
        receive.try_recv()?;
        assert!(matches!(job.status(), Status::Cancelled));
        assert!(
            jobs.spawn(Scope::default(), "late", |_| async { Ok(()) })
                .is_err()
        );
        Ok(())
    }
    #[test]
    fn queued_restore_can_be_cancelled_without_taking_a_permit() -> Result<()> {
        let limit = RestoreLimit::new(1);
        let permit = limit.acquire(&AtomicBool::new(false))?;
        assert!(limit.acquire(&AtomicBool::new(true)).is_err());
        drop(permit);
        limit.acquire(&AtomicBool::new(false))?;
        Ok(())
    }
}
