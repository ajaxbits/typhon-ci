use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

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

enum Msg<Id> {
    Cancel(Id),
    Finish(Id),
    Run(Id, oneshot::Sender<()>, JoinHandle<()>),
    Shutdown,
    Wait(Id, oneshot::Sender<()>),
}

struct TaskHandle {
    canceler: Option<oneshot::Sender<()>>,
    handle: JoinHandle<()>,
    waiters: Vec<oneshot::Sender<()>>,
}

pub struct Tasks<Id> {
    handle: Mutex<Option<JoinHandle<()>>>,
    sender: mpsc::Sender<Msg<Id>>,
}

impl<Id: std::cmp::Eq + std::hash::Hash + std::clone::Clone + Send + Sync + 'static> Tasks<Id> {
    pub fn new() -> Self {
        let (sender, mut receiver) = mpsc::channel(256);
        let handle = tokio::spawn(async move {
            let mut tasks: HashMap<Id, TaskHandle> = HashMap::new();
            while let Some(msg) = receiver.recv().await {
                match msg {
                    Msg::Cancel(id) => {
                        let _ = tasks
                            .get_mut(&id)
                            .map(|task| task.canceler.take().map(|send| send.send(())));
                    }
                    Msg::Finish(id) => {
                        if let Some(task) = tasks.remove(&id) {
                            let _ = task.handle.await;
                            for send in task.waiters {
                                let _ = send.send(());
                            }
                        }
                    }
                    Msg::Run(id, sender, handle) => {
                        let task = TaskHandle {
                            canceler: Some(sender),
                            handle,
                            waiters: Vec::new(),
                        };
                        tasks.insert(id, task);
                    }
                    Msg::Shutdown => {
                        let ids: Vec<_> = tasks.keys().cloned().collect();
                        for id in ids.iter() {
                            tasks
                                .get_mut(&id)
                                .map(|task| task.canceler.take().map(|sender| sender.send(())));
                        }
                        for id in ids {
                            if let Some(mut task) = tasks.remove(&id) {
                                let _ = task.handle.await;
                                let _ = task.waiters.drain(..).map(|sender| sender.send(()));
                            }
                        }
                        break;
                    }
                    Msg::Wait(id, sender) => match tasks.get_mut(&id) {
                        Some(task) => {
                            task.waiters.push(sender);
                        }
                        None => {
                            let _ = sender.send(());
                        }
                    },
                }
            }
        });
        let handle = Mutex::new(Some(handle));
        Self { handle, sender }
    }

    pub async fn wait(&self, id: &Id) -> () {
        let (sender, receiver) = oneshot::channel();
        let _ = self.sender.send(Msg::Wait(id.clone(), sender)).await;
        let _ = receiver.await;
    }

    // TODO: `finish` should be able to output an error
    pub async fn run<
        T: Send + 'static,
        O: Future<Output = T> + Send + 'static,
        U: Future<Output = ()> + Send + 'static,
        F: (FnOnce(Option<T>) -> U) + Send + Sync + 'static,
    >(
        &self,
        id: Id,
        task: O,
        finish: F,
    ) {
        let (send, recv) = oneshot::channel::<()>();
        let sender_self = self.sender.clone();
        let id_bis = id.clone();
        let handle = tokio::spawn(async move {
            let r = tokio::select! {
                _ = recv => None,
                r = task => Some(r),
            };
            finish(r).await;
            let _ = sender_self.send(Msg::Finish(id_bis)).await;
        });
        let _ = self.sender.send(Msg::Run(id, send, handle)).await;
    }

    pub async fn cancel(&self, id: Id) {
        let _ = self.sender.send(Msg::Cancel(id)).await;
    }

    pub async fn shutdown(&'static self) {
        let handle = self.handle.lock().await.take();
        if let Some(handle) = handle {
            if self.sender.send(Msg::Shutdown).await.is_ok() {
                let _ = handle.await;
            } else {
                handle.abort();
            }
        }
    }
}
