//! A multi-consumer, single producer cell that receives notifications when the inner value is
//! changed.
//!
//! # Usage
//!
//! [`Watch::new`] returns a [`Watch`] / [`Store`] pair. These are the consumer and
//! producer halves of the cell. The watch cell is created with an initial
//! value. Calls to [`Watch::borrow`] will always yield the latest value.
//!
//! ```
//! # use futures_watch::*;
//! let (watch, store) = Watch::new("hello");
//! assert_eq!(*watch.borrow(), "hello");
//! # drop(store);
//! ```
//!
//! Using the [`Store`] handle, the cell value can be updated.
//!
//! ```
//! # use futures_watch::*;
//! let (watch, mut store) = Watch::new("hello");
//! store.store("goodbye");
//! assert_eq!(*watch.borrow(), "goodbye");
//! ```
//!
//! [`Watch`] handles are future-aware and will receive notifications whenever
//! the inner value is changed.
//!
//! ```
//! # extern crate futures;
//! # extern crate futures_watch;
//! # pub fn main() {
//! # use futures::*;
//! # use futures_watch::*;
//! # use std::thread;
//! # use std::time::Duration;
//! let (watch, mut store) = Watch::new("hello");
//!
//! thread::spawn(move || {
//!     thread::sleep(Duration::from_millis(100));
//!     store.store("goodbye");
//! });
//!
//! watch.into_future()
//!     .and_then(|(_, watch)| {
//!         assert_eq!(*watch.borrow(), "goodbye");
//!         Ok(())
//!     })
//!     .wait().unwrap();
//! # }
//! ```
//!
//! [`Watch::borrow`] will yield the most recently stored value. All
//! intermediate values are dropped.
//!
//! ```
//! # use futures_watch::*;
//! let (watch, mut store) = Watch::new("hello");
//!
//! store.store("two");
//! store.store("three");
//!
//! assert_eq!(*watch.borrow(), "three");
//! ```
//!
//! # Cancellation
//!
//! [`Store::poll_cancel`] allows the producer to detect when all [`Watch`]
//! handles have been dropped. This indicates that there is no further interest
//! in the values being produced and work can be stopped.
//!
//! When the [`Store`] is dropped, the watch handles will be notified and
//! [`Watch::is_final`] will return true.
//!
//! # Thread safety
//!
//! Both [`Watch`] and [`Store`] are thread safe. They can be moved to other
//! threads and can be used in a concurrent environment. Clones of [`Watch`]
//! handles may be moved to separate threads and also used concurrently.
//!
//! [`Watch`]: struct.Watch.html
//! [`Store`]: struct.Store.html
//! [`Watch::new`]: struct.Watch.html#method.new
//! [`Watch::borrow`]: struct.Watch.html#method.borrow
//! [`Watch::is_final`]: struct.Watch.html#method.is_final
//! [`Store::poll_cancel`]: struct.Store.html#method.poll_cancel

#![deny(warnings, missing_docs, missing_debug_implementations)]

extern crate fnv;
extern crate futures;

use fnv::FnvHashMap;
use futures::{Stream, Sink, Poll, Async, AsyncSink, StartSend};
use futures::task::AtomicTask;

use std::{mem, ops};
use std::sync::{Arc, Weak, Mutex, RwLock, RwLockReadGuard};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering::SeqCst;

/// Uses a `Watch` to produce a `Stream` of mapped values.
pub mod then_stream;

pub use then_stream::Then;

/// A future-aware cell that receives notifications when the inner value is
/// changed.
///
/// `Watch` implements `Stream`, yielding `()` whenever the inner value is
/// changed. This allows a user to monitor this stream to get notified of change
/// events.
///
/// `Watch` handles may be cloned in order to create additional watchers. Each
/// watcher operates independently and can be used to notify separate tasks.
/// Each watcher handle must be used from only a single task.
///
/// See crate level documentation for more details.
#[derive(Debug)]
pub struct Watch<T> {
    /// Pointer to the shared state
    shared: Arc<Shared<T>>,

    /// Pointer to the watcher's internal state
    inner: Arc<WatchInner>,

    /// Watcher ID.
    id: u64,

    /// Last observed version
    ver: usize,
}

/// Update the inner value of a `Watch` cell.
///
/// The [`store`] function sets the inner value of the cell, returning the
/// previous value. Alternatively, `Store` implements `Sink` such that values
/// pushed into the `Sink` are stored in the cell.
///
/// See crate level documentation for more details.
///
/// [`store`]: #method.store
#[derive(Debug)]
pub struct Store<T> {
    shared: Weak<Shared<T>>,
}

/// Borrowed reference
///
/// See [`Watch::borrow`] for more details.
///
/// [`Watch::borrow`]: struct.Watch.html#method.borrow
#[derive(Debug)]
pub struct Ref<'a, T: 'a> {
    inner: RwLockReadGuard<'a, T>,
}

/// Errors produced by `Watch`.
#[derive(Debug)]
pub struct WatchError {
    _p: (),
}

/// Errors produced by `Store`.
#[derive(Debug)]
pub struct StoreError<T> {
    inner: T,
}

#[derive(Debug)]
struct Shared<T> {
    /// The most recent value
    value: RwLock<T>,

    /// The current version
    ///
    /// The lowest bit represents a "closed" state. The rest of the bits
    /// represent the current version.
    version: AtomicUsize,

    /// All watchers
    watchers: Mutex<Watchers>,

    /// Task to notify when all watchers drop
    cancel: AtomicTask,
}

#[derive(Debug)]
struct Watchers {
    next_id: u64,
    watchers: FnvHashMap<u64, Arc<WatchInner>>,
}

#[derive(Debug)]
struct WatchInner {
    task: AtomicTask,
}

const CLOSED: usize = 1;

// ===== impl Watch =====

impl<T> Watch<T> {
    /// Create a new watch cell, returning the consumer / producer halves.
    ///
    /// All values stored by the `Store` will become visible to the `Watch`
    /// handles. Only the last value stored is made available to the `Watch`
    /// half. All intermediate values are dropped.
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures_watch::*;
    /// let (watch, mut store) = Watch::new("hello");
    /// store.store("goodbye");
    /// assert_eq!(*watch.borrow(), "goodbye");
    /// ```
    pub fn new(init: T) -> (Watch<T>, Store<T>) {
        let inner = Arc::new(WatchInner::new());

        // Insert the watcher
        let mut watchers = FnvHashMap::with_capacity_and_hasher(0, Default::default());
        watchers.insert(0, inner.clone());

        let shared = Arc::new(Shared {
            value: RwLock::new(init),
            version: AtomicUsize::new(0),
            watchers: Mutex::new(Watchers {
                next_id: 1,
                watchers,
            }),
            cancel: AtomicTask::new(),
        });

        let store = Store {
            shared: Arc::downgrade(&shared),
        };

        let watch = Watch {
            shared,
            inner,
            id: 0,
            ver: 0,
        };

        (watch, store)
    }

    /// Returns true if the current value represents the final value
    ///
    /// A value becomes final once the `Store` handle is dropped. This indicates
    /// that there can no longer me any values stored in the cell.
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures_watch::*;
    /// let (watch, store) = Watch::new("hello");
    ///
    /// assert!(!watch.is_final());
    /// drop(store);
    /// assert!(watch.is_final());
    /// ```
    pub fn is_final(&self) -> bool {
        CLOSED == self.shared.version.load(SeqCst) & CLOSED
    }

    /// Returns a reference to the inner value
    ///
    /// Outstanding borrows hold a read lock on the inner value. This means that
    /// long lived borrows could cause the produce half to block. It is
    /// recommended to keep the borrow as short lived as possible.
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures_watch::*;
    /// let (watch, _) = Watch::new("hello");
    /// assert_eq!(*watch.borrow(), "hello");
    /// ```
    pub fn borrow(&self) -> Ref<T> {
        let inner = self.shared.value.read().unwrap();
        Ref { inner }
    }

    /// Convert this watch into a stream of values produced by an `M`-typed map function.
    pub fn then_stream<M: Then<T>>(self, then: M) -> then_stream::ThenStream<T, M> {
        then_stream::ThenStream::new(self, then)
    }
}

/// A stream of inner value change events.
///
/// Whenever the inner value of the cell is updated by the `Store` handle, `()`
/// is yielded by this stream.
impl<T> Stream for Watch<T> {
    type Item = ();
    type Error = WatchError;

    fn poll(&mut self) -> Poll<Option<()>, Self::Error> {
        // Make sure the task is up to date
        self.inner.task.register();

        let version = self.shared.version.load(SeqCst);

        if CLOSED == version & CLOSED {
            // The `Store` handle has been dropped.
            return Ok(None.into());
        }

        if self.ver == version {
            return Ok(Async::NotReady);
        }

        // Track the latest version
        self.ver = version;

        Ok(Some(()).into())
    }
}

impl<T> Clone for Watch<T> {
    fn clone(&self) -> Self {
        let inner = Arc::new(WatchInner::new());
        let shared = self.shared.clone();

        let id = {
            let mut watchers = shared.watchers.lock().unwrap();
            let id = watchers.next_id;

            watchers.next_id += 1;
            watchers.watchers.insert(id, inner.clone());

            id
        };

        let ver = self.ver;

        Watch {
            shared: shared,
            inner,
            id,
            ver,
        }
    }
}

impl<T> Drop for Watch<T> {
    fn drop(&mut self) {
        let mut watchers = self.shared.watchers.lock().unwrap();
        watchers.watchers.remove(&self.id);
    }
}

impl WatchInner {
    fn new() -> Self {
        WatchInner { task: AtomicTask::new() }
    }
}

// ===== impl Store =====

impl<T> Store<T> {
    /// Store a new value in the cell, notifying all watchers. The previous
    /// value is returned.
    ///
    /// # Examples
    ///
    /// ```
    /// # use futures_watch::*;
    /// let (watch, mut borrow) = Watch::new("hello");
    /// assert_eq!(borrow.store("goodbye").unwrap(), "hello");
    /// assert_eq!(*watch.borrow(), "goodbye");
    /// ```
    pub fn store(&mut self, value: T) -> Result<T, StoreError<T>> {
        let shared = match self.shared.upgrade() {
            Some(shared) => shared,
            // All `Watch` handles have been canceled
            None => return Err(StoreError::new(value)),
        };

        // Replace the value
        let value = {
            let mut lock = shared.value.write().unwrap();
            mem::replace(&mut *lock, value)
        };

        // Update the version. 2 is used so that the CLOSED bit is not set.
        shared.version.fetch_add(2, SeqCst);

        // Notify all watchers
        notify_all(&*shared);

        // Return the old value
        Ok(value)
    }

    /// Returns `Ready` when all watchers have dropped.
    ///
    /// This allows the producer to get notified when interest in the produced
    /// values is canceled and immediately stop doing work.
    pub fn poll_cancel(&mut self) -> Poll<(), ()> {
        match self.shared.upgrade() {
            Some(shared) => {
                shared.cancel.register();
                Ok(Async::NotReady)
            }
            None => {
                Ok(Async::Ready(()))
            }
        }
    }
}

impl<T> Sink for Store<T> {
    type SinkItem = T;
    type SinkError = StoreError<T>;

    fn start_send(&mut self, item: T) -> StartSend<T, StoreError<T>> {
        let _ = self.store(item)?;
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), StoreError<T>> {
        Ok(().into())
    }
}

/// Notify all watchers of a change
fn notify_all<T>(shared: &Shared<T>) {
    let watchers = shared.watchers.lock().unwrap();

    for watcher in watchers.watchers.values() {
        // Notify the task
        watcher.task.notify();
    }
}

impl<T> Drop for Store<T> {
    fn drop(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            shared.version.fetch_or(CLOSED, SeqCst);
            notify_all(&*shared);
        }
    }
}

// ===== impl Ref =====

impl<'a, T: 'a> ops::Deref for Ref<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.inner.deref()
    }
}

// ===== impl Shared =====

impl<T> Drop for Shared<T> {
    fn drop(&mut self) {
        self.cancel.notify();
    }
}

// ===== impl StoreError =====

impl<T> StoreError<T> {
    fn new(inner: T) -> Self {
        StoreError { inner }
    }
}
