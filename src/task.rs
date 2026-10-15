//! Background work reported to the UI: a label, a stage, and a counter.

use futures::FutureExt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Running,
    Done,
    Failed(String),
}

#[derive(Debug)]
pub struct TaskInner {
    pub label: String,
    pub stage: Mutex<String>,
    pub done: AtomicU64,
    pub total: AtomicU64,
    pub status: Mutex<TaskStatus>,
}

#[derive(Debug, Clone)]
pub struct Task(pub Arc<TaskInner>);

impl Task {
    pub fn new(label: impl Into<String>) -> Self {
        Task(Arc::new(TaskInner {
            label: label.into(),
            stage: Mutex::new(String::new()),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            status: Mutex::new(TaskStatus::Running),
        }))
    }

    pub fn stage(&self, s: impl Into<String>, total: u64) {
        *self.0.stage.lock().unwrap() = s.into();
        self.0.done.store(0, Ordering::Relaxed);
        self.0.total.store(total, Ordering::Relaxed);
    }

    pub fn inc(&self, n: u64) {
        self.0.done.fetch_add(n, Ordering::Relaxed);
    }

    pub fn finish(&self, r: &anyhow::Result<()>) {
        *self.0.status.lock().unwrap() = match r {
            Ok(()) => TaskStatus::Done,
            Err(e) => TaskStatus::Failed(format!("{e:#}")),
        };
    }

    pub fn status(&self) -> TaskStatus {
        self.0.status.lock().unwrap().clone()
    }

    pub fn fraction(&self) -> Option<f32> {
        let total = self.0.total.load(Ordering::Relaxed);
        (total > 0).then(|| self.0.done.load(Ordering::Relaxed) as f32 / total as f32)
    }

    pub fn stage_text(&self) -> String {
        self.0.stage.lock().unwrap().clone()
    }
}

/// A value computed in the background and picked up by the UI later.
pub struct Promise<T>(Arc<Mutex<Option<anyhow::Result<T>>>>);

impl<T> Clone for Promise<T> {
    fn clone(&self) -> Self {
        Promise(self.0.clone())
    }
}

impl<T: Send + 'static> Promise<T> {
    pub fn spawn<F>(rt: &tokio::runtime::Handle, ctx: &egui::Context, fut: F) -> Self
    where
        F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
    {
        let slot = Arc::new(Mutex::new(None));
        let out = slot.clone();
        let ctx = ctx.clone();
        rt.spawn(async move {
            // A panic still fills the slot, so the UI sees a failure instead of waiting forever.
            let r = AssertUnwindSafe(fut).catch_unwind().await.unwrap_or_else(|p| Err(anyhow::anyhow!("internal error: {}", panic_message(&*p))));
            *out.lock().unwrap() = Some(r);
            ctx.request_repaint();
        });
        Promise(slot)
    }

    pub fn ready(&self) -> bool {
        self.0.lock().unwrap().is_some()
    }

    /// Takes the result once it's available.
    pub fn take(&self) -> Option<anyhow::Result<T>> {
        self.0.lock().unwrap().take()
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> &str {
    p.downcast_ref::<&str>().copied().or_else(|| p.downcast_ref::<String>().map(String::as_str)).unwrap_or("task panicked")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panicking_promise_fails_instead_of_hanging() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let p: Promise<()> = Promise::spawn(rt.handle(), &egui::Context::default(), async { panic!("bad hash") });
        let start = std::time::Instant::now();
        while !p.ready() {
            assert!(start.elapsed() < std::time::Duration::from_secs(5), "promise never resolved");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let e = p.take().unwrap().unwrap_err();
        assert!(format!("{e}").contains("bad hash"), "{e}");
    }
}
