//! This library helps async runtimes support the execution of arbitrary futures, by enabling futures to provide their own event polling logic. It is an attempt to implement the approach described by [Context reactor hook](https://jblog.andbit.net/2022/12/28/context-reactor-hook/).
//!
//! There are two integration points:
//! - Futures that need to run their own event polling logic in the execution thread must call [`ContextExt::top_level_poller`] to obtain a [`TopLevelPoller`] and then call [`TopLevelPoller::set_waiter`] to register a [`WakerWaiter`] on it.
//! - Whatever part of the application that is responsible for polling top-level futures (i.e. the async runtime) needs to implement the [`TopLevelPoller`] trait and provide it using [`ContextExt::with_top_level_poller`]. This library provides such an implementation via [`block_on`].
//!
//! Only one [`WakerWaiter`] can be registered on a [`TopLevelPoller`]. If more than one future relies on the same event polling logic, the futures should coordinate and share the same [`WakerWaiter`].
//!
//!
//! # Example of a future registering a `WakerWaiter`
//!
//! ```
//! # use std::future::Future;
//! # use std::pin::Pin;
//! # use std::sync::{Arc, Mutex, Weak};
//! # use std::task::{Context, Poll};
//! # use waker_waiter::{ContextExt, WakerWait, WakerWaiter};
//! #
//! static REACTOR: Mutex<Option<Arc<Reactor>>> = Mutex::new(None);
//!
//! struct Reactor {
//!     waiter: Option<WakerWaiter>,
//! }
//!
//! impl Reactor {
//!     fn current() -> Arc<Reactor> {
//!         let mut reactor = REACTOR.lock().unwrap();
//!
//!         if reactor.is_none() {
//!             let r = Arc::new(Reactor { waiter: None });
//!
//!             let waiter = Arc::new(ReactorWaiter {
//!                 reactor: Arc::downgrade(&r),
//!             }).into();
//!
//!             // SAFETY: nobody else could be borrowing right now
//!             let r = unsafe {
//!                 let r = (Arc::into_raw(r) as *mut Reactor).as_mut().unwrap();
//!                 r.waiter = Some(waiter);
//!
//!                 Arc::from_raw(r as *const Reactor)
//!             };
//!
//!             *reactor = Some(r);
//!         }
//!
//!         Arc::clone(reactor.as_ref().unwrap())
//!     }
//!
//!     fn waiter<'a>(self: &'a Arc<Self>) -> &'a WakerWaiter {
//!         self.waiter.as_ref().unwrap()
//!     }
//! }
//!
//! struct ReactorWaiter {
//!     reactor: Weak<Reactor>,
//! }
//!
//! impl WakerWait for ReactorWaiter {
//!     fn wait(self: &Arc<Self>) {
//!         // ... blocking poll for events ...
//!     }
//!
//!     fn cancel(self: &Arc<Self>) {
//!         // ... some way to unblock the above ...
//!     }
//!
//!     fn can_cancel(self: &Arc<Self>) -> bool {
//!         true
//!     }
//! }
//!
//! struct MyFuture;
//!
//! impl Future for MyFuture {
//! #   type Output = ();
//! #
//!     fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
//!         let p = match cx.top_level_poller() {
//!             Some(p) => p,
//!             None => panic!("MyFuture requires context to provide TopLevelPoller"),
//!         };
//!
//!         if p.set_waiter(Reactor::current().waiter()).is_err() {
//!             panic!("Incompatible waiter already assigned to TopLevelPoller");
//!         }
//!
//!         // ... register waker, perform I/O, etc ...
//! #       unimplemented!();
//!     }
//! }
//! ```
//!
//! # Example of an executor providing a `TopLevelPoller`
//!
//! ```
//! # use std::future::Future;
//! # use std::mem::MaybeUninit;
//! # use std::pin::{pin, Pin};
//! # use std::sync::{Arc, Mutex};
//! # use std::task::{Context, Poll, Wake};
//! # use std::thread::{self, Thread};
//! # use waker_waiter::{ContextExt, SetWaiterError, TopLevelPoller, WakerWaiter};
//! struct ThreadWaker {
//!     thread: Thread,
//!     waiter: Arc<Mutex<Option<WakerWaiter>>>,
//! }
//!
//! impl Wake for ThreadWaker {
//!     fn wake(self: Arc<Self>) {
//!         if thread::current().id() == self.thread.id() {
//!             // if we were woken in the same thread as execution,
//!             // then the wake was caused by the WakerWaiter which
//!             // will return control without any signaling needed
//!             return;
//!         }
//!
//!         let waiter = self.waiter.lock().unwrap().clone();
//!
//!         if let Some(waiter) = waiter {
//!             // if a waiter was configured, then the execution thread
//!             // will be blocking on it and we'll need to unblock it
//!             waiter.cancel();
//!         } else {
//!             // if a waiter was not configured, then the execution
//!             // thread will be asleep with a standard thread park
//!             self.thread.unpark();
//!         }
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct MyTopLevelPoller {
//!     waiter: Arc<Mutex<Option<WakerWaiter>>>,
//! }
//!
//! impl TopLevelPoller for MyTopLevelPoller {
//!     fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError> {
//!         let self_waiter = &mut *self.waiter.lock().unwrap();
//!
//!         if let Some(cur) = self_waiter {
//!             if cur == waiter {
//!                 return Ok(()); // already set to this waiter
//!             } else {
//!                 return Err(SetWaiterError); // already set to a different waiter
//!             }
//!         }
//!
//!         *self_waiter = Some(waiter.clone());
//!
//!         Ok(())
//!     }
//! }
//!
//! let waiter = Arc::new(Mutex::new(None));
//! let waker = Arc::new(ThreadWaker {
//!     thread: thread::current(),
//!     waiter: Arc::clone(&waiter),
//! }).into();
//! let mut cx = Context::from_waker(&waker);
//! let mut poller = MyTopLevelPoller { waiter };
//!
//! let mut fut = pin!(async { /* ... */ });
//!
//! loop {
//!    let result = {
//!        let mut scratch = MaybeUninit::uninit();
//!        let mut cx =
//!            Context::from_waker(&waker).with_top_level_poller(&mut poller, &mut scratch);
//!
//!        fut.as_mut().poll(&mut cx)
//!    };
//!
//!    match result {
//!         Poll::Ready(res) => break res,
//!         Poll::Pending => {
//!             let waiter = poller.waiter.lock().unwrap().clone();
//!
//!             // if a waiter is configured then block on it. else do a
//!             // standard thread park
//!             match waiter {
//!                 Some(waiter) => waiter.wait(),
//!                 None => thread::park(),
//!             }
//!         }
//!     }
//! }
//! ```

#![feature(waker_getters)]

use std::fmt;
use std::mem::{self, ManuallyDrop, MaybeUninit};
use std::sync::Arc;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};

#[derive(PartialEq)]
pub struct RawWaiterVTable {
    clone: unsafe fn(*const ()) -> RawWaiter,
    wait: unsafe fn(*const ()),
    cancel: unsafe fn(*const ()),
    can_cancel: unsafe fn(*const ()) -> bool,
    drop: unsafe fn(*const ()),
}

impl RawWaiterVTable {
    pub const fn new(
        clone: unsafe fn(_: *const ()) -> RawWaiter,
        wait: unsafe fn(_: *const ()),
        cancel: unsafe fn(_: *const ()),
        can_cancel: unsafe fn(_: *const ()) -> bool,
        drop: unsafe fn(_: *const ()),
    ) -> Self {
        Self {
            clone,
            wait,
            cancel,
            can_cancel,
            drop,
        }
    }
}

#[derive(PartialEq)]
pub struct RawWaiter {
    data: *const (),
    vtable: &'static RawWaiterVTable,
}

impl RawWaiter {
    #[inline]
    pub const fn new(data: *const (), vtable: &'static RawWaiterVTable) -> Self {
        Self { data, vtable }
    }
}

#[derive(PartialEq)]
pub struct WakerWaiter {
    inner: RawWaiter,
}

impl WakerWaiter {
    pub fn wait(&self) {
        unsafe { (self.inner.vtable.wait)(self.inner.data) }
    }

    pub fn cancel(&self) {
        unsafe { (self.inner.vtable.cancel)(self.inner.data) }
    }

    pub fn can_cancel(&self) -> bool {
        unsafe { (self.inner.vtable.can_cancel)(self.inner.data) }
    }

    pub fn to_local(self) -> LocalWakerWaiter {
        let data = self.inner.data;
        let vtable = self.inner.vtable;

        mem::forget(self);

        LocalWakerWaiter {
            inner: RawWaiter { data, vtable },
        }
    }
}

impl Clone for WakerWaiter {
    fn clone(&self) -> Self {
        Self {
            inner: unsafe { (self.inner.vtable.clone)(self.inner.data) },
        }
    }
}

impl Drop for WakerWaiter {
    fn drop(&mut self) {
        unsafe { (self.inner.vtable.drop)(self.inner.data) }
    }
}

unsafe impl Send for WakerWaiter {}
unsafe impl Sync for WakerWaiter {}

#[derive(PartialEq)]
pub struct LocalWakerWaiter {
    inner: RawWaiter,
}

impl LocalWakerWaiter {
    pub fn wait(&self) {
        unsafe { (self.inner.vtable.wait)(self.inner.data) }
    }

    pub fn cancel(&self) {
        unsafe { (self.inner.vtable.cancel)(self.inner.data) }
    }

    pub fn can_cancel(&self) -> bool {
        unsafe { (self.inner.vtable.can_cancel)(self.inner.data) }
    }
}

impl Clone for LocalWakerWaiter {
    fn clone(&self) -> Self {
        Self {
            inner: unsafe { (self.inner.vtable.clone)(self.inner.data) },
        }
    }
}

impl Drop for LocalWakerWaiter {
    fn drop(&mut self) {
        unsafe { (self.inner.vtable.drop)(self.inner.data) }
    }
}

pub trait WakerWait {
    fn wait(self: &Arc<Self>);
    fn cancel(self: &Arc<Self>);
    fn can_cancel(self: &Arc<Self>) -> bool;
}

impl<W: WakerWait + Send + Sync + 'static> From<Arc<W>> for WakerWaiter {
    fn from(waiter: Arc<W>) -> WakerWaiter {
        Self {
            inner: raw_waiter(waiter),
        }
    }
}

#[inline(always)]
fn raw_waiter<W: WakerWait + Send + Sync + 'static>(waiter: Arc<W>) -> RawWaiter {
    struct VTablePerType<W>(W);

    impl<W: WakerWait + Send + Sync + 'static> VTablePerType<W> {
        const VTABLE: &'static RawWaiterVTable = &RawWaiterVTable::new(
            clone_waiter::<W>,
            wait::<W>,
            cancel::<W>,
            can_cancel::<W>,
            drop_waiter::<W>,
        );
    }

    unsafe fn clone_waiter<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) -> RawWaiter {
        unsafe { Arc::increment_strong_count(waiter as *const W) };
        RawWaiter::new(waiter as *const (), VTablePerType::<W>::VTABLE)
    }

    unsafe fn wait<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) {
        let waiter = unsafe { ManuallyDrop::new(Arc::from_raw(waiter as *const W)) };
        <W as WakerWait>::wait(&waiter);
    }

    unsafe fn cancel<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) {
        let waiter = unsafe { ManuallyDrop::new(Arc::from_raw(waiter as *const W)) };
        <W as WakerWait>::cancel(&waiter);
    }

    unsafe fn can_cancel<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) -> bool {
        let waiter = unsafe { ManuallyDrop::new(Arc::from_raw(waiter as *const W)) };
        <W as WakerWait>::can_cancel(&waiter)
    }

    unsafe fn drop_waiter<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) {
        unsafe { Arc::decrement_strong_count(waiter as *const W) };
    }

    RawWaiter::new(
        Arc::into_raw(waiter) as *const (),
        VTablePerType::<W>::VTABLE,
    )
}

#[derive(Debug)]
pub struct SetWaiterError;

impl fmt::Display for SetWaiterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Failed to set waiter: conflict")
    }
}

#[derive(Debug)]
pub enum SetLocalWaiterError {
    Conflict,
    Unsupported,
}

impl fmt::Display for SetLocalWaiterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Conflict => write!(f, "Failed to set local waiter: conflict"),
            Self::Unsupported => write!(f, "Failed to set local waiter: unsupported"),
        }
    }
}

pub trait TopLevelPoller {
    fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError>;

    fn set_local_waiter(&mut self, _waiter: &LocalWakerWaiter) -> Result<(), SetLocalWaiterError> {
        Err(SetLocalWaiterError::Unsupported)
    }
}

struct WakerWithTopLevelPoller<'a> {
    inner: Waker,
    poller: &'a mut dyn TopLevelPoller,
}

unsafe fn clone_fn(data: *const ()) -> RawWaker {
    let w = (data as *const WakerWithTopLevelPoller).as_ref().unwrap();

    let inner = ManuallyDrop::new(w.inner.clone());
    let inner_raw = inner.as_raw();

    RawWaker::new(inner_raw.data(), inner_raw.vtable())
}

unsafe fn wake_fn(data: *const ()) {
    let data = data as *mut WakerWithTopLevelPoller;
    let w = Box::from_raw(data);

    w.inner.wake();
}

unsafe fn wake_by_ref_fn(data: *const ()) {
    let w = (data as *const WakerWithTopLevelPoller).as_ref().unwrap();

    w.inner.wake_by_ref();
}

unsafe fn drop_fn(data: *const ()) {
    let data = data as *mut WakerWithTopLevelPoller;
    drop(Box::from_raw(data));
}

const VTABLE: RawWakerVTable = RawWakerVTable::new(clone_fn, wake_fn, wake_by_ref_fn, drop_fn);

impl<'a> WakerWithTopLevelPoller<'a> {
    fn new(waker: Waker, poller: &'a mut dyn TopLevelPoller) -> Self {
        Self {
            inner: waker,
            poller,
        }
    }

    // caller must ensure poller outlives returned Waker
    unsafe fn into_std(self) -> Waker {
        let data = Box::new(self);
        let rw = RawWaker::new(Box::into_raw(data) as *const (), &VTABLE);

        unsafe { Waker::from_raw(rw) }
    }

    fn try_from_raw(rw: &'a RawWaker) -> Option<&'a mut Self> {
        if rw.vtable() == &VTABLE {
            unsafe {
                Some(
                    (rw.data() as *mut WakerWithTopLevelPoller)
                        .as_mut()
                        .unwrap(),
                )
            }
        } else {
            None
        }
    }
}

pub trait ContextExt<'a> {
    fn with_top_level_poller<'b: 'a>(
        self,
        poller: &'b mut dyn TopLevelPoller,
        scratch: &'a mut MaybeUninit<Waker>,
    ) -> Self;
    fn top_level_poller(&mut self) -> Option<&mut dyn TopLevelPoller>;
    fn with_waker<'b>(
        &'b mut self,
        waker: &'b Waker,
        scratch: &'b mut MaybeUninit<Waker>,
    ) -> Context<'b>;
}

impl<'a> ContextExt<'a> for Context<'a> {
    fn with_top_level_poller<'b: 'a>(
        self,
        poller: &'b mut dyn TopLevelPoller,
        scratch: &'a mut MaybeUninit<Waker>,
    ) -> Self {
        let waker = WakerWithTopLevelPoller::new(self.waker().clone(), poller);

        // SAFETY: poller outlives waker
        let waker = unsafe { waker.into_std() };

        scratch.write(waker);

        // SAFETY: data is initialized
        let waker = unsafe { scratch.assume_init_ref() };

        Self::from_waker(waker)
    }

    fn top_level_poller(&mut self) -> Option<&mut dyn TopLevelPoller> {
        match WakerWithTopLevelPoller::try_from_raw(self.waker().as_raw()) {
            Some(w) => Some(w.poller),
            None => None,
        }
    }

    fn with_waker<'b>(
        &'b mut self,
        waker: &'b Waker,
        scratch: &'b mut MaybeUninit<Waker>,
    ) -> Context<'b> {
        match WakerWithTopLevelPoller::try_from_raw(self.waker().as_raw()) {
            Some(w) => {
                let waker = WakerWithTopLevelPoller::new(waker.clone(), w.poller);

                // SAFETY: poller outlives waker
                let waker = unsafe { waker.into_std() };

                scratch.write(waker);

                // SAFETY: data is initialized
                let waker = unsafe { scratch.assume_init_ref() };

                Context::from_waker(waker)
            }
            None => Context::from_waker(waker),
        }
    }
}

pub trait WakerExt {
    fn will_wake2(&self, other: &Waker) -> bool;
}

impl WakerExt for Waker {
    fn will_wake2(&self, other: &Waker) -> bool {
        match WakerWithTopLevelPoller::try_from_raw(self.as_raw()) {
            Some(w) => w.inner.will_wake(other),
            None => self.will_wake(other),
        }
    }
}

mod executor {
    use super::{ContextExt, SetWaiterError, TopLevelPoller, WakerWaiter};
    use std::future::Future;
    use std::mem::MaybeUninit;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Wake};
    use std::thread::{self, Thread};

    struct ThreadWaker {
        thread: Thread,
        waiter: Arc<Mutex<Option<WakerWaiter>>>,
    }

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            if thread::current().id() == self.thread.id() {
                // if we were woken in the same thread as execution,
                // then the wake was caused by the WakerWaiter which
                // will return control without any signaling needed
                return;
            }

            let waiter = self.waiter.lock().unwrap().clone();

            if let Some(waiter) = waiter {
                // if a waiter was configured, then the execution thread
                // will be blocking on it and we'll need to unblock it
                waiter.cancel();
            } else {
                // if a waiter was not configured, then the execution
                // thread will be asleep with a standard thread park
                self.thread.unpark();
            }
        }
    }

    #[derive(Clone)]
    struct MyTopLevelPoller {
        waiter: Arc<Mutex<Option<WakerWaiter>>>,
    }

    impl TopLevelPoller for MyTopLevelPoller {
        fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError> {
            let self_waiter = &mut *self.waiter.lock().unwrap();

            if let Some(cur) = self_waiter {
                if cur == waiter {
                    return Ok(()); // already set to this waiter
                } else {
                    return Err(SetWaiterError); // already set to a different waiter
                }
            }

            *self_waiter = Some(waiter.clone());

            Ok(())
        }
    }

    pub fn block_on<T>(fut: T) -> T::Output
    where
        T: Future,
    {
        let waiter = Arc::new(Mutex::new(None));
        let mut poller = MyTopLevelPoller {
            waiter: Arc::clone(&waiter),
        };

        let waker = Arc::new(ThreadWaker {
            thread: thread::current(),
            waiter,
        })
        .into();

        let mut fut = pin!(fut);

        let res = loop {
            let result = {
                let mut scratch = MaybeUninit::uninit();
                let mut cx =
                    Context::from_waker(&waker).with_top_level_poller(&mut poller, &mut scratch);

                fut.as_mut().poll(&mut cx)
            };

            match result {
                Poll::Ready(res) => break res,
                Poll::Pending => {
                    let waiter = poller.waiter.lock().unwrap().clone();

                    // if a waiter is configured then block on it. else do a
                    // standard thread park
                    match waiter {
                        Some(waiter) => waiter.wait(),
                        None => thread::park(),
                    }
                }
            }
        };

        res
    }
}

pub use executor::block_on;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::pin::{pin, Pin};
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    struct NoopWaker;

    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    struct MyTopLevelPoller {
        waiter: RefCell<Option<WakerWaiter>>,
    }

    impl TopLevelPoller for MyTopLevelPoller {
        fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError> {
            let self_waiter = &mut *self.waiter.borrow_mut();

            if let Some(cur) = self_waiter {
                if cur == waiter {
                    return Ok(()); // already set to this waiter
                } else {
                    return Err(SetWaiterError); // already set to a different waiter
                }
            }

            *self_waiter = Some(waiter.clone());

            Ok(())
        }
    }

    struct NoopWakerWaiter;

    impl WakerWait for NoopWakerWaiter {
        fn wait(self: &Arc<Self>) {}
        fn cancel(self: &Arc<Self>) {}
        fn can_cancel(self: &Arc<Self>) -> bool {
            false
        }
    }

    struct MyFuture {
        waiter: WakerWaiter,
    }

    impl MyFuture {
        fn new() -> Self {
            Self {
                waiter: Arc::new(NoopWakerWaiter).into(),
            }
        }
    }

    impl Future for MyFuture {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            // first, apply WakerWaiter
            // NOTE: alternatively, the lookup/set could happen when MyFuture
            // is constructed (or when some parent owner is constructed)

            let waker = Arc::new(NoopWaker).into();
            let mut scratch = MaybeUninit::uninit();
            let mut cx1 = cx.with_waker(&waker, &mut scratch);

            // should be cheap: looking up a thread local
            // NOTE: later on, this could become a property of Context
            let p = match cx1.top_level_poller() {
                Some(p) => p,
                None => panic!("MyFuture requires context to provide TopLevelPoller"),
            };

            // can be cheap: it's up to the setter impl, but this
            // could just be a pointer comparison
            if p.set_waiter(&self.waiter).is_err() {
                panic!("Incompatible WakerWaiter already assigned to context's TopLevelPoller");
            }

            Poll::Ready(())
        }
    }

    #[test]
    fn test_context_inherit() {
        let waker = Arc::new(NoopWaker).into();

        let mut poller = MyTopLevelPoller {
            waiter: RefCell::new(None),
        };

        let mut scratch = MaybeUninit::uninit();
        let mut cx = Context::from_waker(&waker).with_top_level_poller(&mut poller, &mut scratch);

        assert!(cx.waker().will_wake2(&waker));
        assert!(cx.top_level_poller().is_some());

        {
            let waker = Arc::new(NoopWaker).into();
            let mut scratch = MaybeUninit::uninit();
            let mut cx2 = cx.with_waker(&waker, &mut scratch);

            assert!(cx2.waker().will_wake2(&waker));
            assert!(cx2.top_level_poller().is_some());
        }

        assert!(cx.waker().will_wake2(&waker));
        assert!(cx.top_level_poller().is_some());
    }

    #[test]
    fn test_waiter() {
        let waker = Arc::new(NoopWaker).into();

        let mut poller = MyTopLevelPoller {
            waiter: RefCell::new(None),
        };

        let mut fut = pin!(MyFuture::new());

        loop {
            let result = {
                let mut scratch = MaybeUninit::uninit();
                let mut cx =
                    Context::from_waker(&waker).with_top_level_poller(&mut poller, &mut scratch);

                fut.as_mut().poll(&mut cx)
            };

            match result {
                Poll::Ready(res) => break res,
                Poll::Pending => match &*poller.waiter.borrow() {
                    Some(waiter) => waiter.wait(),
                    None => {}
                },
            }
        }
    }

    #[test]
    fn test_block_on() {
        block_on(MyFuture::new());
    }
}
