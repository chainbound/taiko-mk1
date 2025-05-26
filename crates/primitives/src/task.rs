use std::{
    collections::HashMap,
    pin::Pin,
    task::{Context, Poll},
};

use futures::FutureExt;
use tokio::task::{JoinError, JoinHandle};

/// A wrapper over a [`HashMap`] of long-running tasks, each represented by a
/// [`tokio::task::JoinHandle`].
///
/// It is possile to await these tasks. Since they are long-running, this future should never
/// return. As such, receiving a value means that a task has completed due to
/// (probably) an error, so that the caller can handle it.
#[derive(Debug, Default)]
pub struct CriticalTasks {
    tasks: HashMap<String, JoinHandle<()>>,
}

impl CriticalTasks {
    /// Creates a new instance of `Self`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Spawn the task and adds it to the list of long-running tasks
    pub fn add_task<F>(&mut self, task: F, name: &str)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let handle = tokio::spawn(task);
        self.tasks.insert(name.to_owned(), handle);
    }

    /// Adds a task handle to the list of long-running tasks.
    pub fn add_handle(&mut self, handle: JoinHandle<()>, name: &str) {
        self.tasks.insert(name.to_owned(), handle);
    }
}

/// The result of awaiting a task in the [`CriticalTasks`] struct.
#[derive(Debug)]
pub struct TaskResult {
    /// The name of the task.
    name: String,
    /// The error, if any.
    err: Option<JoinError>,
}

impl TaskResult {
    /// Creates a new instance of `TaskResult`.
    pub const fn new(name: String, err: Option<JoinError>) -> Self {
        Self { name, err }
    }

    /// Returns the name of the task.
    #[allow(clippy::missing_const_for_fn)]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the error message, or an empty string if there was no error.
    pub fn error_message(self) -> String {
        let default = String::new();

        let Some(err) = self.err else {
            return default;
        };

        if err.is_panic() {
            let panic_value = err.into_panic();

            if let Some(s) = panic_value.downcast_ref::<&str>() {
                return (*s).to_owned()
            } else if let Some(s) = panic_value.downcast_ref::<String>() {
                return s.clone()
            }
            return "Task panicked with unknown type".to_owned()
        }

        default
    }
}

impl Future for CriticalTasks {
    type Output = Option<TaskResult>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.tasks.is_empty() {
            return Poll::Ready(None);
        }

        for (name, task) in &mut this.tasks {
            match task.poll_unpin(cx) {
                Poll::Ready(res) => {
                    let result = TaskResult::new(name.clone(), res.err());
                    return Poll::Ready(Some(result));
                }
                Poll::Pending => {
                    // Task is still running, continue polling
                }
            }
        }

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::task;

    // Helper to spawn a task that panics with the given value and return its JoinError
    async fn panic_with_value<T: Send + 'static>(value: T) -> JoinError {
        let handle = task::spawn(async move { std::panic::panic_any(value) });
        handle.await.expect_err("task should panic")
    }

    #[tokio::test]
    async fn error_message_from_str_panic() {
        let err = panic_with_value("boom").await;
        let res = TaskResult::new("t".to_owned(), Some(err));
        assert_eq!(res.error_message(), "boom");
    }

    #[tokio::test]
    async fn error_message_from_string_panic() {
        let err = panic_with_value("boom".to_owned()).await;
        let res = TaskResult::new("t".to_owned(), Some(err));
        assert_eq!(res.error_message(), "boom");
    }

    #[tokio::test]
    async fn error_message_from_unknown_panic() {
        let err = panic_with_value(42u32).await;
        let res = TaskResult::new("t".to_owned(), Some(err));
        assert_eq!(res.error_message(), "Task panicked with unknown type");
    }

    #[tokio::test]
    async fn error_message_no_panic() {
        let handle = task::spawn(async {});
        let res = TaskResult::new("t".to_owned(), handle.await.err());
        assert_eq!(res.error_message(), "");
    }
}
