

 
//! Types for spawning and waiting on groups of tasks.
//!
//! This crate provides onthe [TaskCollection](crate::TaskCollection) for the
//! grouping of spawned tasks. The TaskCollection type is created using a
//! [Spawner](crate::Spawner) implementation. Any tasks spawned via the
//! TaskCollections `spawn` method are tracked with minimal overhead.
//! `await`ing the TaskCollection will yield until all the spawned tasks for
//! that collection have completed.
//!
//! The following example shows how to use a TaskCollection to wait for spawned tasks to finish:
//!
//! ```rust
//! # use task_collection::TaskCollection;
//! # use futures::channel::mpsc;
//! # use futures::StreamExt;
//! # use core::time::Duration;
//!
//! fn main() {
//!     let runtime = tokio::runtime::Runtime::new().unwrap();
//!     let (tx, mut rx) = mpsc::unbounded::<u64>();
//!     
//!     runtime.spawn(async move {
//!         (0..10).for_each(|v| {
//!             tx.unbounded_send(v).expect("Failed to send");
//!         })
//!     });
//!     
//!     runtime.block_on(async {
//!         let collection = TaskCollection::new(&runtime);
//!         while let Some(val) = rx.next().await {
//!             collection.spawn(async move {
//!                 // Simulate some async work
//!                 tokio::time::sleep(Duration::from_secs(val)).await;
//!                 println!("Value {}", val);
//!             });
//!         }
//!
//!         collection.await;
//!         println!("All values printed");
//!     });
//! }
//! ```

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

use core::task::Poll;
use core::{future::Future, sync::atomic::AtomicUsize};
use core::{ops::Deref, sync::atomic::Ordering::SeqCst};

use futures_util::task::AtomicWaker;

pub struct TaskCollection<S, T> {
    spawner: S,
    tracker: T,
}

impl<S> TaskCollection<S, ()>
where
    S: Spawner,
{
    pub fn with_static_tracker(
        spawner: S,
        tracker: &'static Tracker,
    ) -> TaskCollection<S, &'static Tracker> {
        TaskCollection { spawner, tracker }
    }

    #[cfg(feature = "alloc")]
    pub fn new(spawner: S) -> TaskCollection<S, alloc::sync::Arc<Tracker>> {
        TaskCollection {
            spawner,
            tracker: alloc::sync::Arc::new(Tracker::new()),
        }
    }
}

impl<S, T> TaskCollection<S, T>
where
    S: Spawner,
    T: 'static + Deref<Target = Tracker> + Clone + Send,
{
    pub fn spawn<F, R>(&self, future: F)
    where
        F: Future<Output = R> + Send + 'static,
    {
        let tracker = self.create_task();
        self.spawner.spawn(async {
            let _ = future.await;
            core::mem::drop(tracker);
        });
    }

    fn create_task(&self) -> Task<T> {
        let mut current_tasks = self.tracker.active_tasks.load(SeqCst);

        loop {
            if current_tasks == usize::MAX {
                panic!();
            }

            let new_tasks = current_tasks + 1;

            let actual_current =
                self.tracker
                    .active_tasks
                    .compare_and_swap(current_tasks, new_tasks, SeqCst);

            if current_tasks == actual_current {
                return Task {
                    inner: self.tracker.clone(),
                };
            }

            current_tasks = actual_current;
        }
    }
}

impl<S, T> Future for TaskCollection<S, T>
where
    T: core::ops::Deref<Target = Tracker>,
{
    type Output = ();

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        let active_tasks = self.tracker.active_tasks.load(SeqCst);

        if active_tasks == 0 {
            Poll::Ready(())
        } else {
            self.tracker.waker.register(cx.waker());

            let active_tasks = self.tracker.active_tasks.load(SeqCst);
            if active_tasks == 0 {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }
}

struct Task<T>
where
    T: Deref<Target = Tracker>,
{
    inner: T,
}

impl<T> Drop for Task<T>
where
    T: Deref<Target = Tracker>,
{
    fn drop(&mut self) {
        let previous = self.inner.active_tasks.fetch_sub(1, SeqCst);

        if previous == 1 {
            self.inner.waker.wake();
        }
    }
}

pub struct Tracker {
    waker: AtomicWaker,
    active_tasks: AtomicUsize,
}

impl Tracker {
    pub const fn new() -> Tracker {
        Tracker {
            waker: AtomicWaker::new(),
            active_tasks: AtomicUsize::new(0),
        }
    }
}

pub trait Spawner {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static;
}

#[cfg(feature = "smol")]
impl Spawner for &smol::Executor<'_> {
    fn spawn<F>(&self, future: F)
    where
        F: core::future::Future<Output = ()> + Send + 'static,
    {
        smol::Executor::spawn(self, future).detach();
    }
}

#[cfg(feature = "tokio")]
impl Spawner for &tokio::runtime::Runtime {
    fn spawn<F>(&self, future: F)
    where
        F: core::future::Future<Output = ()> + Send + 'static,
    {
        tokio::runtime::Runtime::spawn(self, future);
    }
}

#[cfg(feature = "tokio")]
pub struct GlobalTokioSpawner;

#[cfg(feature = "tokio")]
impl Spawner for GlobalTokioSpawner {
    fn spawn<F>(&self, future: F)
    where
        F: core::future::Future<Output = ()> + Send + 'static,
    {
        tokio::spawn(future);
    }
}

#[cfg(feature = "async-std")]
pub struct AsyncStdSpawner;

#[cfg(feature = "async-std")]
impl Spawner for AsyncStdSpawner {
    fn spawn<F>(&self, future: F)
    where
        F: core::future::Future<Output = ()> + Send + 'static,
    {
        async_std::task::spawn(future);
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::{TaskCollection, Tracker};
    use core::panic;
    use smol::future::FutureExt;
    use std::time::Duration;

    #[test]
    #[cfg(feature = "smol")]
    fn test_smol() {
        let exec = smol::Executor::new();

        let f = async {
            let collection = TaskCollection::new(&exec);

            for i in &[5, 3, 1, 4, 2] {
                collection.spawn(async move {
                    smol::Timer::after(Duration::from_secs(*i)).await;
                });
            }

            collection.await;
        };

        let timeout = async {
            smol::Timer::after(Duration::from_secs(10)).await;
            panic!();
        };

        smol::block_on(exec.run(f.or(timeout)));
    }

    #[test]
    #[cfg(feature = "smol")]
    fn test_smol_static() {
        let exec = smol::Executor::new();
        static T: Tracker = Tracker::new();
        let f = async {
            let collection = TaskCollection::with_static_tracker(&exec, &T);

            for i in &[5, 3, 1, 4, 2] {
                collection.spawn(async move {
                    smol::Timer::after(Duration::from_secs(*i)).await;
                });
            }

            collection.await;
        };

        let timeout = async {
            smol::Timer::after(Duration::from_secs(10)).await;
            panic!();
        };

        smol::block_on(exec.run(f.or(timeout)));
    }

    #[test]
    #[cfg(feature = "tokio")]
    fn test_tokio() {
        let runtime = tokio::runtime::Runtime::new().unwrap();

        let f = async {
            let collection = TaskCollection::new(&runtime);

            for i in &[5, 3, 1, 4, 2] {
                collection.spawn(async move {
                    tokio::time::sleep(Duration::from_secs(*i)).await;
                });
            }

            collection.await;
        };

        runtime.block_on(async {
            tokio::select! {
                _ = f => (),
                _ = tokio::time::sleep(Duration::from_secs(10)) => panic!()
            }
        });
    }

    #[test]
    #[cfg(feature = "async-std")]
    fn test_async_std() {
        use crate::AsyncStdSpawner;
        let f = async {
            let collection = TaskCollection::new(AsyncStdSpawner);

            for i in &[5, 3, 1, 4, 2] {
                collection.spawn(async move {
                    async_std::task::sleep(Duration::from_secs(*i)).await;
                });
            }

            collection.await;
        };

        async_std::task::block_on(async_std::future::timeout(Duration::from_secs(10), f)).unwrap();
    }
}