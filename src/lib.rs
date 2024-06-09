//! This library helps async runtimes support the execution of arbitrary futures, by enabling futures to provide their own event polling logic. It is an attempt to implement the approach described by [Context reactor hook](https://jblog.andbit.net/2022/12/28/context-reactor-hook/).
//!
//! There are two integration points:
//! - Futures that need to run their own event polling logic in the execution thread must call [`get_poller`] to obtain a [`TopLevelPoller`] and then call [`TopLevelPoller::set_waiter`] to register a [`WakerWaiter`] on it.
//! - Whatever part of the application that is responsible for polling top-level futures (i.e. the async runtime) needs to implement the [`TopLevelPoller`] trait and provide it using [`std::task::ContextBuilder::ext`]. This library provides such an implementation via [`block_on`].
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
//! # use waker_waiter::{TopLevelPoller, WakerWait, WakerWaiter, WakerWaiterCanceler, get_poller};
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
//!         todo!();
//!     }
//!
//!     fn canceler(self: &Arc<Self>) -> WakerWaiterCanceler {
//!         // ... provide a way to unblock the above ...
//!         todo!();
//!     }
//! }
//!
//! struct MyFuture;
//!
//! impl Future for MyFuture {
//! #   type Output = ();
//! #
//!     fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
//!         let p = match get_poller(cx) {
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
//! #![feature(local_waker)]
//! #![feature(context_ext)]
//! # use std::future::Future;
//! # use std::mem::MaybeUninit;
//! # use std::pin::{pin, Pin};
//! # use std::sync::{Arc, Mutex};
//! # use std::task::{Context, ContextBuilder, Poll, Wake};
//! # use std::thread::{self, Thread};
//! # use waker_waiter::{Anyable, SetWaiterError, TopLevelPoller, WakerWaiter};
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
//!             waiter.canceler().cancel();
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
//!        let mut a = Anyable::new(&mut poller as &mut dyn TopLevelPoller);
//!        let mut cx = ContextBuilder::from_waker(&waker).ext(a.as_any()).build();
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

#![feature(local_waker)]
#![feature(context_ext)]

use std::fmt;
use std::mem::{self, transmute, ManuallyDrop};
use std::ptr;
use std::sync::Arc;
use std::task::Context;

pub struct RawCancelerVTable {
    clone: unsafe fn(*const ()) -> RawCanceler,
    cancel: unsafe fn(*const ()),
    drop: unsafe fn(*const ()),
}

impl RawCancelerVTable {
    pub const fn new(
        clone: unsafe fn(_: *const ()) -> RawCanceler,
        cancel: unsafe fn(_: *const ()),
        drop: unsafe fn(_: *const ()),
    ) -> Self {
        Self {
            clone,
            cancel,
            drop,
        }
    }
}

pub struct RawCanceler {
    data: *const (),
    vtable: &'static RawCancelerVTable,
}

impl RawCanceler {
    #[inline]
    pub const fn new(data: *const (), vtable: &'static RawCancelerVTable) -> Self {
        Self { data, vtable }
    }
}

pub struct WakerWaiterCanceler {
    inner: RawCanceler,
}

impl WakerWaiterCanceler {
    pub fn cancel(&self) {
        unsafe { (self.inner.vtable.cancel)(self.inner.data) }
    }

    fn into_raw(mut self) -> RawCanceler {
        const NOOP_VTABLE: &'static RawCancelerVTable =
            &RawCancelerVTable::new(|c| RawCanceler::new(c, NOOP_VTABLE), |_| {}, |_| {});

        mem::replace(&mut self.inner, RawCanceler::new(ptr::null(), NOOP_VTABLE))
    }
}

impl Clone for WakerWaiterCanceler {
    fn clone(&self) -> Self {
        Self {
            inner: unsafe { (self.inner.vtable.clone)(self.inner.data) },
        }
    }
}

impl Drop for WakerWaiterCanceler {
    fn drop(&mut self) {
        unsafe { (self.inner.vtable.drop)(self.inner.data) }
    }
}

unsafe impl Send for WakerWaiterCanceler {}
unsafe impl Sync for WakerWaiterCanceler {}

#[derive(PartialEq)]
pub struct RawWaiterVTable {
    clone: unsafe fn(*const ()) -> RawWaiter,
    wait: unsafe fn(*const ()),
    canceler: unsafe fn(*const ()) -> Option<RawCanceler>,
    drop: unsafe fn(*const ()),
}

impl RawWaiterVTable {
    pub const fn new(
        clone: unsafe fn(_: *const ()) -> RawWaiter,
        wait: unsafe fn(_: *const ()),
        canceler: unsafe fn(_: *const ()) -> Option<RawCanceler>,
        drop: unsafe fn(_: *const ()),
    ) -> Self {
        Self {
            clone,
            wait,
            canceler,
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

    pub fn canceler(&self) -> WakerWaiterCanceler {
        let raw = unsafe { (self.inner.vtable.canceler)(self.inner.data).unwrap() };

        WakerWaiterCanceler { inner: raw }
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

    pub fn canceler(&self) -> Option<WakerWaiterCanceler> {
        let raw = unsafe { (self.inner.vtable.canceler)(self.inner.data) };

        raw.map(|v| WakerWaiterCanceler { inner: v })
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
    fn canceler(self: &Arc<Self>) -> WakerWaiterCanceler;
}

impl<W: WakerWait + Send + Sync + 'static> From<Arc<W>> for WakerWaiter {
    fn from(waiter: Arc<W>) -> Self {
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
            canceler::<W>,
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

    unsafe fn canceler<W: WakerWait + Send + Sync + 'static>(
        waiter: *const (),
    ) -> Option<RawCanceler> {
        let waiter = unsafe { ManuallyDrop::new(Arc::from_raw(waiter as *const W)) };

        Some(<W as WakerWait>::canceler(&waiter).into_raw())
    }

    unsafe fn drop_waiter<W: WakerWait + Send + Sync + 'static>(waiter: *const ()) {
        unsafe { Arc::decrement_strong_count(waiter as *const W) };
    }

    RawWaiter::new(
        Arc::into_raw(waiter) as *const (),
        VTablePerType::<W>::VTABLE,
    )
}

pub trait WakerWaiterCancel {
    fn cancel(self: &Arc<Self>);
}

impl<C: WakerWaiterCancel + Send + Sync + 'static> From<Arc<C>> for WakerWaiterCanceler {
    fn from(canceler: Arc<C>) -> Self {
        Self {
            inner: raw_canceler(canceler),
        }
    }
}

#[inline(always)]
fn raw_canceler<C: WakerWaiterCancel + Send + Sync + 'static>(canceler: Arc<C>) -> RawCanceler {
    struct VTablePerType<C>(C);

    impl<C: WakerWaiterCancel + Send + Sync + 'static> VTablePerType<C> {
        const VTABLE: &'static RawCancelerVTable =
            &RawCancelerVTable::new(clone_canceler::<C>, cancel::<C>, drop_canceler::<C>);
    }

    unsafe fn clone_canceler<C: WakerWaiterCancel + Send + Sync + 'static>(
        canceler: *const (),
    ) -> RawCanceler {
        unsafe { Arc::increment_strong_count(canceler as *const C) };

        RawCanceler::new(canceler as *const (), VTablePerType::<C>::VTABLE)
    }

    unsafe fn cancel<C: WakerWaiterCancel + Send + Sync + 'static>(canceler: *const ()) {
        let canceler = unsafe { ManuallyDrop::new(Arc::from_raw(canceler as *const C)) };
        <C as WakerWaiterCancel>::cancel(&canceler);
    }

    unsafe fn drop_canceler<C: WakerWaiterCancel + Send + Sync + 'static>(canceler: *const ()) {
        unsafe { Arc::decrement_strong_count(canceler as *const C) };
    }

    RawCanceler::new(
        Arc::into_raw(canceler) as *const (),
        VTablePerType::<C>::VTABLE,
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

pub mod ctx_ref {
    use std::any::Any;
    use std::marker::PhantomData;
    use std::task::Context;

    // SAFETY: only implement on dyn types that don't expose
    // lifetimes held by the type. i.e. traits with methods
    // that only work with argument lifetimes. this allows
    // us to safely cast any held lifetimes, because there
    // is no way to move-in, move-out, or obtain a reference
    // to data with any such held lifetimes
    pub unsafe trait DynWithStaticInterface {
        type Static: ?Sized + 'static;
        type Clamped<'a>: ?Sized
        where
            Self: 'a;

        // return type is safe to use because it does not expose
        // whatever lifetimes were converted to 'static
        fn to_static(&mut self) -> &mut Self::Static;

        // return type is safe to use because it does not expose
        // whatever lifetimes were converted to 'a
        fn to_clamped<'a>(&'a mut self) -> &'a mut Self::Clamped<'a>;
    }

    struct AnyablePrivate<T: ?Sized>(*mut T);

    pub struct Anyable<'a, T: ?Sized, U: ?Sized> {
        p: AnyablePrivate<T>,
        _marker: PhantomData<&'a mut U>,
    }

    impl<'a, T: DynWithStaticInterface + ?Sized> Anyable<'a, T::Static, T> {
        pub fn new(r: &'a mut T) -> Self {
            Self {
                p: AnyablePrivate(r.to_static()),
                _marker: PhantomData,
            }
        }
    }

    impl<T: ?Sized + 'static, U: ?Sized> Anyable<'_, T, U> {
        pub fn as_any<'a>(&'a mut self) -> &'a mut (dyn Any + 'static) {
            &mut self.p
        }
    }

    pub fn ctx_downcast<'a, T: DynWithStaticInterface + ?Sized + 'static>(
        cx: &mut Context<'a>,
    ) -> Option<&'a mut T::Clamped<'a>> {
        let a = cx.ext();

        let p = match a.downcast_mut::<AnyablePrivate<T>>() {
            Some(p) => p,
            None => return None,
        };

        // SAFETY: the inner pointer is guaranteed to be valid because
        // p has the same lifetime as the reference that was
        // originally casted to the pointer
        let r: &'a mut T = unsafe { p.0.as_mut().unwrap() };

        Some(r.to_clamped())
    }
}

use ctx_ref::{ctx_downcast, DynWithStaticInterface};

pub use ctx_ref::Anyable;

// SAFETY: implementing on a dyn type that does not expose any lifetimes held by the type
unsafe impl DynWithStaticInterface for (dyn TopLevelPoller + '_) {
    type Static = dyn TopLevelPoller + 'static;
    type Clamped<'a> = dyn TopLevelPoller + 'a;

    fn to_static(&mut self) -> &mut Self::Static {
        unsafe { transmute(self) }
    }

    fn to_clamped<'a>(&'a mut self) -> &'a mut Self::Clamped<'a> {
        unsafe { transmute(self) }
    }
}

pub fn get_poller<'a>(cx: &mut Context<'a>) -> Option<&'a mut (dyn TopLevelPoller + 'a)> {
    ctx_downcast::<dyn TopLevelPoller>(cx)
}

mod executor {
    use super::Anyable;
    use super::{SetWaiterError, TopLevelPoller, WakerWaiter, WakerWaiterCanceler};
    use std::future::Future;
    use std::pin::pin;
    use std::sync::{Arc, Mutex};
    use std::task::{ContextBuilder, Poll, Wake};
    use std::thread::{self, Thread};

    struct ThreadWaker {
        thread: Thread,
        waiter_data: Arc<Mutex<Option<(WakerWaiter, WakerWaiterCanceler)>>>,
    }

    impl Wake for ThreadWaker {
        fn wake(self: Arc<Self>) {
            if thread::current().id() == self.thread.id() {
                // if we were woken in the same thread as execution,
                // then the wake was caused by the WakerWaiter which
                // will return control without any signaling needed
                return;
            }

            let waiter_data = self.waiter_data.lock().unwrap().clone();

            if let Some(waiter_data) = waiter_data {
                // if a waiter was configured, then the execution thread
                // will be blocking on it and we'll need to unblock it
                waiter_data.1.cancel();
            } else {
                // if a waiter was not configured, then the execution
                // thread will be asleep with a standard thread park
                self.thread.unpark();
            }
        }
    }

    #[derive(Clone)]
    pub struct MyTopLevelPoller {
        waiter_data: Arc<Mutex<Option<(WakerWaiter, WakerWaiterCanceler)>>>,
    }

    impl TopLevelPoller for MyTopLevelPoller {
        fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError> {
            let waiter_data = &mut *self.waiter_data.lock().unwrap();

            if let Some(cur) = waiter_data {
                if cur.0 == *waiter {
                    return Ok(()); // already set to this waiter
                } else {
                    return Err(SetWaiterError); // already set to a different waiter
                }
            }

            *waiter_data = Some((waiter.clone(), waiter.canceler()));

            Ok(())
        }
    }

    pub fn block_on<T>(fut: T) -> T::Output
    where
        T: Future,
    {
        let waiter_data = Arc::new(Mutex::new(None));

        let mut poller = MyTopLevelPoller {
            waiter_data: Arc::clone(&waiter_data),
        };

        let waker = Arc::new(ThreadWaker {
            thread: thread::current(),
            waiter_data,
        })
        .into();

        let mut fut = pin!(fut);

        let res = loop {
            let result = {
                let mut a = Anyable::new(&mut poller as &mut dyn TopLevelPoller);
                let mut cx = ContextBuilder::from_waker(&waker).ext(a.as_any()).build();

                fut.as_mut().poll(&mut cx)
            };

            match result {
                Poll::Ready(res) => break res,
                Poll::Pending => {
                    let waiter_data = poller.waiter_data.lock().unwrap().clone();

                    // if a waiter is configured then block on it. else do a
                    // standard thread park
                    match waiter_data {
                        Some(waiter_data) => waiter_data.0.wait(),
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
    use std::task::{ContextBuilder, Poll, Wake};

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

    struct NoopWakerWaiterCanceler;

    impl WakerWaiterCancel for NoopWakerWaiterCanceler {
        fn cancel(self: &Arc<Self>) {}
    }

    struct NoopWakerWaiter;

    impl WakerWait for NoopWakerWaiter {
        fn wait(self: &Arc<Self>) {}
        fn canceler(self: &Arc<Self>) -> WakerWaiterCanceler {
            Arc::new(NoopWakerWaiterCanceler).into()
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
            let mut cx1 = ContextBuilder::from(cx).waker(&waker).build();

            // should be cheap
            let poller = match get_poller(&mut cx1) {
                Some(p) => p,
                None => panic!("MyFuture requires context to provide TopLevelPoller"),
            };

            // can be cheap: it's up to the setter impl, but this
            // could just be a pointer comparison
            if poller.set_waiter(&self.waiter).is_err() {
                panic!("Incompatible WakerWaiter already assigned to context's TopLevelPoller");
            }

            Poll::Ready(())
        }
    }

    struct MyFuture2 {
        waiter: WakerWaiter,
    }

    impl MyFuture2 {
        fn new() -> Self {
            Self {
                waiter: Arc::new(NoopWakerWaiter).into(),
            }
        }
    }

    impl Future for MyFuture2 {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            // first, apply WakerWaiter
            // NOTE: alternatively, the lookup/set could happen when MyFuture
            // is constructed (or when some parent owner is constructed)

            let waker = Arc::new(NoopWaker).into();
            let mut cx1 = ContextBuilder::from(cx).waker(&waker).build();

            // should be cheap
            let poller = match get_poller(&mut cx1) {
                Some(p) => p,
                None => panic!("MyFuture requires context to provide TopLevelPoller"),
            };

            // can be cheap: it's up to the setter impl, but this
            // could just be a pointer comparison
            if poller.set_waiter(&self.waiter).is_err() {
                panic!("Incompatible WakerWaiter already assigned to context's TopLevelPoller");
            }

            Poll::Ready(())
        }
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
                let mut a = Anyable::new(&mut poller as &mut dyn TopLevelPoller);
                let mut cx = ContextBuilder::from_waker(&waker).ext(a.as_any()).build();

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
        block_on(MyFuture2::new());
    }
}
