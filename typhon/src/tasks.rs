use tokio::sync::oneshot::{channel, Sender};
use tokio::sync::Mutex;

use std::collections::HashMap;
use std::future::Future;

#[derive(Debug)]
pub enum Error {
    ShuttingDown,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Task manager is shutting down")
    }
}

#[derive(Debug)]
struct TaskHandle {
    canceler: Option<Sender<()>>,
    waiters: Vec<Sender<()>>,
}

#[derive(Debug)]
struct TasksUnwrapped<Id> {
    handles: HashMap<Id, TaskHandle>,
    shutdown: bool,
}

#[derive(Debug)]
pub struct Tasks<Id> {
    tasks: Mutex<TasksUnwrapped<Id>>,
}

impl<Id: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Send + Sync> Tasks<Id> {
    pub fn new() -> Self {
        Tasks {
            tasks: Mutex::new(TasksUnwrapped {
                handles: HashMap::new(),
                shutdown: false,
            }),
        }
    }

    pub async fn wait(&self, id: &Id) -> () {
        let mut tasks = self.tasks.lock().await;
        let (send, recv) = channel::<()>();
        match tasks.handles.get_mut(&id) {
            Some(task) => {
                task.waiters.push(send);
            }
            None => {
                let _ = send.send(());
            }
        }
        drop(tasks);
        let _ = recv.await;
    }

    pub async fn is_running(&self, id: &Id) -> bool {
        let tasks = self.tasks.lock().await;
        tasks.handles.get(&id).is_some()
    }

    pub async fn run<
        S: Send + 'static,
        T: Future<Output = S> + Send + 'static,
        U: Future<Output = ()> + Send + 'static,
        F: FnOnce(Option<S>) -> U + Send + 'static,
    >(
        &'static self,
        id: Id,
        task: T,
        f: F,
    ) -> Result<(), Error> {
        let mut tasks = self.tasks.lock().await;
        if tasks.shutdown {
            return Err(Error::ShuttingDown);
        }
        let (send, recv) = channel::<()>();
        let handle = TaskHandle {
            canceler: Some(send),
            waiters: Vec::new(),
        };
        tasks.handles.insert(id.clone(), handle);
        drop(tasks);
        tokio::spawn(async move {
            let r = tokio::select! {
                _ = recv => None,
                r = task => Some(r),
            };
            f(r).await;
            self.tasks.lock().await.handles.remove(&id).map(|handle| {
                for send in handle.waiters {
                    let _ = send.send(());
                }
            });
        });
        Ok(())
    }

    pub async fn cancel(&self, id: &Id) -> bool {
        self.tasks
            .lock()
            .await
            .handles
            .get_mut(&id)
            .map(|task| task.canceler.take().map(|send| send.send(())))
            .flatten()
            .is_some()
    }

    pub async fn shutdown(&'static self) {
        let mut tasks = self.tasks.lock().await;
        tasks.shutdown = true;
        let mut set = tokio::task::JoinSet::new();
        for id in tasks.handles.keys() {
            set.spawn({
                let id = id.clone();
                async move { self.wait(&id).await }
            });
            self.cancel(id).await;
        }
        drop(tasks);
        while let Some(_) = set.join_next().await {}
    }
}
