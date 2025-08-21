//! Async & reactive synchronization model to keep multiple async tasks / threads partially
//! synchronized.
//!
//! ## Differentiation From Traditional Asnyc Streams
//! **Important:** An observable is not a clonable `Stream<T>` – versions may be skipped on the
//! receiving side, if it doesnt ask for updates anymore or if updates are published to quickly the
//! receiving observable just retrieves the latest value.
//!
//! This is a powerful concept since it allows you to just skip the versions which are outdated by
//! a newer version anyway and hence gain some performance advantage through the lazyness implied
//! by this concept. Although the performance aspect is probably unimportant in most usecases it
//! allows you to write simpler code since you dont need to take your position in the stream into
//! account.
//!
//! ## Examples
//! ### Sharing A Counter Between Tasks
//! ```rust
//! use async_std::task::spawn;
//! use async_observable::Observable;
//!
//! #[async_std::main]
//! async fn main() {
//!     let mut observable = Observable::new(0);
//!     let mut tasks = vec![];
//!
//!     for i in 0..10 {
//!         let mut fork = observable.clone();
//!
//!         tasks.push(spawn(async move {
//!             let update = fork.next().await;
//!
//!             println!(
//!                 "Task {} was notified about updated observable {}",
//!                 i, update
//!             );
//!         }));
//!     }
//!
//!     observable.publish(1);
//!
//!     for t in tasks {
//!         t.await
//!     }
//! }
//! ```
use futures::stream::Stream;
use slab::Slab;
use std::{
    fmt,
    ops::DerefMut,
    sync::{Arc, Mutex, MutexGuard},
    task::{Poll, Waker},
};

/// The initial version of a tracked value
///
/// Note: This is 1, to be able to use 0 as an indicator that the
/// version tracker has been reset.
const INITIAL_VERSION: u128 = 1;

/// Wraps a value and lets you fork the state to synchronize it between tasks and threads.
///
/// ## Creating New Observables
/// There are several ways to create a new observable, altough using the `new` function should be
/// the preferred way.
///
/// ```rust
/// # use async_observable::Observable;
/// let mut using_new = Observable::new(0);
/// let mut using_from = Observable::from(0);
/// let mut using_into: Observable<u8> = 0.into();
/// ```
///
/// ## Publishing New Values
/// Publishing a new version is done by a single call to the `publish()` method.
///
/// ```rust
/// # use async_observable::Observable;
/// # let mut observable = Observable::new(0);
/// observable.publish(1);
/// observable.publish(2);
/// observable.publish(3);
/// ```
///
/// ## Receiving Updates
///
/// ```rust
/// # use async_observable::Observable;
/// # async {
/// let mut observable = Observable::new(0);
/// let mut fork = observable.clone();
///
/// observable.publish(1);
/// observable.publish(2);
/// observable.publish(3);
///
/// assert_eq!(fork.next().await, 3);
/// # };
/// ```
///
/// ### Important
/// **Keep in mind that if you publish multiple versions directly after each other there no
/// guarantees that all forked observables will receive every change!** But as long as every
/// observable is constently asking for changes (via `next()`) you are guaranteed that every
/// observable received the latest version.
pub struct Observable<T>
where
    T: Clone,
{
    uuid: uuid::Uuid,
    inner: Arc<Mutex<Inner<T>>>,
    version: u128,
    waker_id: Option<usize>,
}

impl<T> Clone for Observable<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            uuid: uuid::Uuid::new_v4(),
            inner: self.inner.clone(),
            version: self.version,
            waker_id: None,
        }
    }
}

impl<T> Observable<T>
where
    T: Clone,
{
    /// Create a new observable from any value.
    pub fn new(value: T) -> Self {
        Observable {
            uuid: uuid::Uuid::new_v4(),
            inner: Arc::new(Mutex::new(Inner::new(value))),
            version: INITIAL_VERSION,
            waker_id: None,
        }
    }

    /// Store provided value and notify forks.
    pub fn publish(&mut self, value: T) {
        self.modify(|v| *v = value);
    }

    /// Modify the underlying value and notify forks.
    pub fn modify<M>(&mut self, modify: M)
    where
        M: FnOnce(&mut T),
    {
        self.modify_conditional(|_| true, modify);
    }

    /// If the condition is met, modify the underlying value and notify forks.
    ///
    /// Returns `true` if the modification was executed.
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let mut observable = Observable::new(0);
    /// let mut fork = observable.clone();
    ///
    /// observable.modify_conditional(|i| *i == 0, |i| *i = 1); // modify
    /// assert_eq!(fork.next().await, 1);
    ///
    /// observable.modify_conditional(|i| *i == 0, |i| *i = 2); // doesnt modify
    /// fork.next().await; // runs forever
    /// # };
    /// ```
    pub fn modify_conditional<C, M>(&mut self, condition: C, modify: M) -> bool
    where
        C: FnOnce(&T) -> bool,
        M: FnOnce(&mut T),
    {
        self.apply(|value| {
            if condition(value) {
                modify(value);
                true
            } else {
                false
            }
        })
    }

    /// Optionally apply the change retrieved by the provided closure.
    ///
    /// Returns `true` if a change was made.
    ///
    /// ```ignore
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut a, mut b) = Observable::new(0).split();
    ///
    /// a.apply(|_| false); // Has no effect
    ///
    /// b.next().await; // runs forever!
    /// # };
    /// ```
    ///
    /// ```ignore
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut a, mut b) = Observable::new(0).split();
    ///
    /// a.apply(|value| {
    ///     *value = 1;
    ///     true
    /// });
    ///
    /// assert_eq!(b.next().await, 1);
    /// # };
    /// ```
    #[doc(hidden)]
    pub(crate) fn apply<F>(&mut self, change: F) -> bool
    where
        F: FnOnce(&mut T) -> bool,
    {
        let mut inner = self.lock();

        if !change(&mut inner.value) {
            return false;
        }

        inner.version += 1;

        for ref waker in inner.waker.iter() {
            waker.1.wake_by_ref();
        }

        inner.waker.clear();

        true
    }

    /// Same as clone, but *the reset causes the fork to instantly have a change available* with the
    /// current state.
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let mut observable = Observable::new(0);
    /// let mut fork = observable.clone_and_reset();
    ///
    /// assert_eq!(fork.next().await, 0);
    /// # };
    /// ```
    pub fn clone_and_reset(&self) -> Observable<T> {
        Self {
            uuid: uuid::Uuid::new_v4(),
            inner: self.inner.clone(),
            version: 0,
            waker_id: None,
        }
    }

    /// Resets the observable to instantly have a change available
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut observable, mut fork) = Observable::new(0).split();
    ///
    /// fork.reset();
    ///
    /// assert_eq!(fork.next().await, 0);
    /// # };
    /// ```
    pub fn reset(&mut self) {
        self.version = 0;
    }

    /// Creates a clone of latest version of the observable value, *without consuming the change!*
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let mut observable = Observable::new(0);
    /// let mut fork = observable.clone_and_reset();
    ///
    /// observable.publish(1);
    ///
    /// assert_eq!(fork.latest(), 1);
    /// assert_eq!(fork.next().await, 1);
    /// # };
    /// ```
    pub fn latest(&self) -> T {
        let inner = self.lock();
        inner.value.clone()
    }

    /// Wait until a new version of the observable was published and return a
    /// clone of the new version.
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut a, mut b) = Observable::new(0).split();
    ///
    /// a.publish(1);
    /// assert_eq!(b.next().await, 1);
    ///
    /// a.publish(2);
    /// assert_eq!(b.next().await, 2);
    ///
    /// b.next().await; // runs forever!
    /// # };
    /// ```
    #[inline]
    pub async fn next(&mut self) -> T {
        futures::StreamExt::next(self)
            .await
            .expect("internal implementation error: observable update streams cannot end")
    }

    /// Skip any potential updates and retrieve the latest version of the
    /// observed value.
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut a, mut b) = Observable::new(0).split();
    ///
    /// a.publish(1);
    /// a.publish(2);
    /// a.publish(3);
    ///
    /// assert_eq!(b.synchronize(), 3);
    ///
    /// b.next().await; // runs forever!
    /// # };
    /// ```
    pub fn synchronize(&mut self) -> T {
        let (value, version) = {
            let inner = self.lock();
            (inner.value.clone(), inner.version)
        };

        self.version = version;
        value
    }

    /// Splits the observable into two handles to the same value
    ///
    /// This is very useful if you are spawning threads or tasks which get an
    /// owned instance of the observable
    ///
    /// ```rust
    /// # use async_observable::Observable;
    /// # async {
    /// let (mut main, mut task) = Observable::new(0).split();
    ///
    /// async_std::task::spawn(async move {
    ///     task.publish(1);
    /// });
    ///
    /// assert_eq!(main.next().await, 1);
    /// # };
    /// ```
    pub fn split(self) -> (Self, Self) {
        (self.clone(), self)
    }

    pub(crate) fn lock(&self) -> MutexGuard<Inner<T>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(e) => e.into_inner(),
        }
    }

    #[cfg(test)]
    pub(crate) fn waker_count(&self) -> usize {
        self.inner.lock().unwrap().waker.len()
    }
}

impl<T> Observable<T>
where
    T: Clone + PartialEq,
{
    /// Publish a change if the new value differs from the current one.
    ///
    /// Returns `true` if a change was made.
    pub fn publish_if_changed(&mut self, value: T) -> bool {
        self.apply(|v| {
            if *v != value {
                *v = value;
                true
            } else {
                false
            }
        })
    }
}

impl<T> PartialEq for Observable<T>
where
    T: Clone + PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        self.latest() == other.latest()
    }
}

impl<T> Eq for Observable<T> where T: Clone + PartialEq + Eq {}

impl<T> From<T> for Observable<T>
where
    T: Clone,
{
    /// Create a new observable from any value. Same as calling `new`.
    fn from(value: T) -> Self {
        Observable::new(value)
    }
}

impl<T> fmt::Debug for Observable<T>
where
    T: Clone + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let inner = self.lock();

        f.debug_struct("Observable")
            .field("inner", &inner)
            .field("version", &self.version)
            .finish()
    }
}

impl<T> Stream for Observable<T>
where
    T: Clone,
{
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        println!(
            "async-observable::{}::stream::preguard {:#?}",
            self.uuid, self.waker_id
        );
        let mut guard = self.lock();
        println!(
            "async-observable::{}::stream::postguard {:#?}",
            self.uuid, self.waker_id
        );
        let inner = guard.deref_mut();

        if self.version == inner.version {
            if let Some(waker) = self.waker_id {
                inner.waker.try_remove(waker);
            }

            let waker_id = inner.waker.insert(cx.waker().clone());

            drop(guard);

            self.waker_id = Some(waker_id);

            println!(
                "async-observable::{}::stream::pending (waker = {waker_id}, version = {})",
                self.uuid, self.version
            );
            Poll::Pending
        } else {
            if let Some(waker) = self.waker_id {
                inner.waker.try_remove(waker);
            }

            let (version, value) = (inner.version, inner.value.clone());

            drop(guard);

            self.waker_id = None;
            self.version = version;

            println!(
                "async-observable::{}::stream::ready, waker none, version = {version}",
                self.uuid
            );
            Poll::Ready(Some(value))
        }
    }
}

impl<T> Drop for Observable<T>
where
    T: Clone,
{
    fn drop(&mut self) {
        if let Some(waker) = self.waker_id {
            println!("async-observable::{}::drop::preguard", self.uuid);
            let mut guard = self.lock();
            println!("async-observable::{}::drop::postguard", self.uuid);
            let inner = guard.deref_mut();
            inner.waker.try_remove(waker);
        }
    }
}

#[cfg(feature = "serde")]
/// Serializes the observable to the latest value
impl<T> serde::Serialize for Observable<T>
where
    T: serde::Serialize + Clone,
{
    #[inline]
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.latest().serialize(serializer)
    }
}

#[cfg(feature = "serde")]
/// Deserializes the value and wraps it into an observable
impl<'de, T> serde::Deserialize<'de> for Observable<T>
where
    T: Clone + serde::Deserialize<'de>,
{
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Into::into)
    }
}

struct Inner<T>
where
    T: Clone,
{
    version: u128,
    value: T,
    waker: Slab<Waker>,
}

impl<T> Inner<T>
where
    T: Clone,
{
    fn new(value: T) -> Self {
        Self {
            version: INITIAL_VERSION,
            value,
            waker: Slab::new(),
        }
    }
}

impl<T> fmt::Debug for Inner<T>
where
    T: Clone + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Inner")
            .field("value", &self.value)
            .field("version", &self.version)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use super::Observable;
    use async_std::future::timeout;
    use async_std::task::{sleep, spawn};
    use std::time::Duration;

    const SLEEP_DURATION: Duration = Duration::from_millis(25);
    const TIMEOUT_DURATION: Duration = Duration::from_millis(500);

    mod publishing {
        use super::*;
        use async_std::test;

        #[test]
        async fn should_get_notified_sync() {
            let mut int = Observable::new(1);
            let mut other = int.clone();

            int.publish(2);
            assert_eq!(other.next().await, 2);
            int.publish(3);
            assert_eq!(other.next().await, 3);
            int.publish(0);
            assert_eq!(other.next().await, 0);
        }

        #[test]
        async fn should_get_notified_sync_multiple() {
            let mut int = Observable::new(1);
            let mut fork_one = int.clone();
            let mut fork_two = int.clone();

            int.publish(2);
            assert_eq!(fork_one.next().await, 2);
            assert_eq!(fork_two.next().await, 2);

            int.publish(3);
            assert_eq!(fork_one.next().await, 3);
            assert_eq!(fork_two.next().await, 3);

            int.publish(0);
            assert_eq!(fork_one.next().await, 0);
            assert_eq!(fork_two.next().await, 0);
        }

        #[test]
        async fn should_publish_after_modify() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.modify(|i| *i += 1);
            assert_eq!(fork.next().await, 2);

            int.modify(|i| *i += 1);
            assert_eq!(fork.next().await, 3);

            int.modify(|i| *i -= 2);
            assert_eq!(fork.next().await, 1);

            int.modify(|i| *i -= 2);
            assert_eq!(fork.next().await, -1);
        }

        #[test]
        async fn should_conditionally_modify() {
            let mut int = Observable::new(1);

            let modified = int.modify_conditional(|i| i % 2 == 0, |i| *i *= 2);
            assert!(!modified);
            assert_eq!(int.latest(), 1);

            let modified = int.modify_conditional(|i| i % 2 == 1, |i| *i *= 2);
            assert!(modified);
            assert_eq!(int.latest(), 2);

            let modified = int.modify_conditional(|i| i % 2 == 0, |i| *i = 1000);
            assert!(modified);
            assert_eq!(int.latest(), 1000);
        }

        #[test]
        async fn shouldnt_publish_same_change() {
            let mut int = Observable::new(1);
            let published = int.publish_if_changed(1);
            assert!(!published);
            assert!(timeout(TIMEOUT_DURATION, int.next()).await.is_err());
        }

        #[test]
        async fn should_publish_changed() {
            let mut int = Observable::new(1);

            let published = int.publish_if_changed(2);
            assert!(published);
            assert_eq!(int.synchronize(), 2);

            let published = int.publish_if_changed(2);
            assert!(!published);
            assert!(timeout(TIMEOUT_DURATION, int.next()).await.is_err());
        }
    }

    mod versions {
        use super::*;
        use async_std::test;

        #[test]
        async fn should_skip_versions() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);
            int.publish(3);
            int.publish(0);

            assert_eq!(fork.next().await, 0);
        }

        #[test]
        async fn should_wait_after_skiped_versions() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);
            int.publish(3);
            int.publish(0);

            assert_eq!(fork.next().await, 0);
            assert!(timeout(TIMEOUT_DURATION, fork.next()).await.is_err());
        }

        #[test]
        async fn should_skip_unchecked_updates() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);
            assert_eq!(fork.next().await, 2);
            int.publish(3);
            int.publish(0);
            assert_eq!(fork.next().await, 0);
        }

        #[test]
        async fn should_clone_and_reset() {
            let int = Observable::new(1);
            let mut fork = int.clone_and_reset();
            assert_eq!(fork.next().await, 1);
        }

        #[test]
        async fn should_reset() {
            let (_int, mut fork) = Observable::new(1).split();
            fork.reset();
            assert_eq!(fork.next().await, 1);
        }
    }

    mod asynchronous {
        use super::*;
        use async_std::test;

        #[test]
        async fn should_wait_for_publisher_task() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            spawn(async move {
                sleep(SLEEP_DURATION).await;
                int.publish(2);
                sleep(SLEEP_DURATION).await;
                int.publish(3);
                sleep(SLEEP_DURATION).await;
                int.publish(0);
            });

            assert_eq!(fork.next().await, 2);
            assert_eq!(fork.next().await, 3);
            assert_eq!(fork.next().await, 0);
        }
    }

    mod synchronization {
        use super::*;
        use async_std::test;

        #[test]
        async fn should_get_latest_without_loosing_updates() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);

            assert_eq!(fork.latest(), 2);
            assert_eq!(fork.latest(), 2);

            assert_eq!(fork.next().await, 2);
        }

        #[test]
        async fn should_skip_updates_while_synchronizing() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);
            int.publish(3);

            assert_eq!(fork.synchronize(), 3);

            assert!(timeout(TIMEOUT_DURATION, fork.next()).await.is_err());
        }

        #[test]
        async fn should_synchronize_multiple_times() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            int.publish(2);
            int.publish(3);

            assert_eq!(fork.synchronize(), 3);
            assert_eq!(fork.synchronize(), 3);

            int.publish(4);

            assert_eq!(fork.synchronize(), 4);

            assert!(timeout(TIMEOUT_DURATION, fork.next()).await.is_err());
        }
    }

    mod future {
        use super::*;
        use async_std::test;

        #[test]
        async fn should_remove_waker_after_resolving() {
            let mut int = Observable::new(1);
            let mut fork = int.clone();

            for _ in 0..100 {
                int.publish(1);
                timeout(Duration::from_millis(10), fork.next()).await.ok();

                assert_eq!(fork.waker_id, None);
                assert_eq!(int.waker_count(), 0);
            }
        }

        #[test]
        async fn should_wait_forever() {
            let int = Observable::new(1);
            let mut fork = int.clone();

            assert!(timeout(TIMEOUT_DURATION, fork.next()).await.is_err());
        }
    }

    #[cfg(feature = "serde")]
    mod serde {
        use super::*;
        use async_std::test;
        use serde_derive::*;

        #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
        struct Foo {
            uint: Observable<u8>,
            string: Observable<String>,
        }

        #[test]
        async fn should_serialize_and_deserialize() {
            let data = Foo {
                uint: 1.into(),
                string: "bar".to_owned().into(),
            };

            let serialized: String = serde_json::to_string(&data).unwrap();
            assert_eq!(serialized, r#"{"uint":1,"string":"bar"}"#);

            let deserialized: Foo = serde_json::from_str(&serialized).unwrap();
            assert_eq!(
                deserialized,
                Foo {
                    uint: 1.into(),
                    string: "bar".to_owned().into()
                }
            );
        }

        #[test]
        async fn should_serialize_latest() {
            let (uint, mut other) = Observable::new(1).split();

            let data = Foo {
                uint,
                string: "bar".to_owned().into(),
            };

            other.publish(2);

            let serialized: String = serde_json::to_string(&data).unwrap();
            assert_eq!(serialized, r#"{"uint":2,"string":"bar"}"#);
        }
    }
}
