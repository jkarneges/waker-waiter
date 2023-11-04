//! This library helps async runtimes support the execution of arbitrary futures, by enabling futures to provide their own event polling logic. It is an attempt to implement the approach described by [Context reactor hook](https://jblog.andbit.net/2022/12/28/context-reactor-hook/), except using thread locals instead of modifying [`std::task::Context`]. The hook essentially provides a thread-level effect, so having to resort to thread locals here is not limiting.
//!
//! There are two integration points:
//! - Futures that need to run their own event polling logic in the execution thread must call [`with_top_level_poller`] and then [`TopLevelPoller::set_waiter`] to register a [`WakerWaiter`].
//! - Whatever part of the application is responsible for polling top-level futures (i.e. the async runtime) needs to implement the [`TopLevelPoller`] trait and call [`set_top_level_poller`] to make it discoverable. This library provides such an implementation via [`block_on`].
//!
//! Only one [`WakerWaiter`] can be registerd on a [`TopLevelPoller`]. If more than one future relies on the same event polling logic, the futures should coordinate and share the same [`WakerWaiter`].
//!
//!
//! # Example of a future registering a `WakerWaiter`
//!
//! ```
//! # use std::future::Future;
//! # use std::pin::Pin;
//! # use std::sync::{Arc, Mutex, Weak};
//! # use std::task::{Context, Poll};
//! # use waker_waiter::{WakerWait, WakerWaiter};
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
//!             let waiter = WakerWaiter::new(Arc::new(ReactorWaiter {
//!                 reactor: Arc::downgrade(&r),
//!             }));
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
//!     fn wait(self: Arc<Self>) {
//!         // ... blocking poll for events ...
//!     }
//!
//!     fn cancel(self: Arc<Self>) {
//!         // ... some way to unblock the above ...
//!     }
//! }
//!
//! struct MyFuture;
//!
//! impl Future for MyFuture {
//! #   type Output = ();
//! #
//!     fn poll(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Self::Output> {
//!         waker_waiter::with_top_level_poller(|p| {
//!             let p = match p {
//!                 Some(p) => p,
//!                 None => panic!("MyFuture requires thread to provide TopLevelPoller"),
//!             };
//!
//!             if p.set_waiter(Reactor::current().waiter()).is_err() {
//!                 panic!("Incompatible waiter already assigned to TopLevelPoller");
//!             }
//!         });
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
//! # use std::pin::{pin, Pin};
//! # use std::sync::{Arc, Mutex};
//! # use std::task::{Context, Poll, Wake};
//! # use std::thread::{self, Thread};
//! # use waker_waiter::{SetWaiterError, TopLevelPoller, WakerWaiter};
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
//! let poller = MyTopLevelPoller { waiter };
//!
//! waker_waiter::set_top_level_poller(Some(poller.clone()));
//!
//! let mut fut = pin!(async { /* ... */ });
//!
//! loop {
//!     match fut.as_mut().poll(&mut cx) {
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

use std::cell::RefCell;
use std::fmt;
use std::mem::{ManuallyDrop, MaybeUninit};
use std::ptr;
use std::sync::Arc;
use std::task::{Context, RawWaker, RawWakerVTable, Waker};

pub trait WakerWait: Send + Sync {
    fn wait(self: Arc<Self>);
    fn cancel(self: Arc<Self>);
}

#[derive(Clone)]
pub struct WakerWaiter(Arc<dyn WakerWait>);

impl WakerWaiter {
    pub fn new(wait: Arc<dyn WakerWait>) -> Self {
        Self(wait)
    }

    pub fn wait(&self) {
        Arc::clone(&self.0).wait();
    }

    pub fn cancel(&self) {
        Arc::clone(&self.0).cancel();
    }
}

impl PartialEq for WakerWaiter {
    fn eq(&self, other: &Self) -> bool {
        let a = Arc::as_ptr(&self.0) as *const ();
        let b = Arc::as_ptr(&other.0) as *const ();

        ptr::eq(a, b)
    }
}

#[derive(Debug)]
pub struct SetWaiterError;

impl fmt::Display for SetWaiterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Failed to set waiter")
    }
}

pub trait TopLevelPoller {
    fn set_waiter(&mut self, waiter: &WakerWaiter) -> Result<(), SetWaiterError>;
}

thread_local! {
    static POLLER: RefCell<Option<Box<dyn TopLevelPoller>>> = RefCell::new(None);
}

pub fn set_top_level_poller<T: TopLevelPoller + 'static>(t: Option<T>) {
    POLLER.with(|r| {
        r.replace(match t {
            Some(t) => Some(Box::new(t)),
            None => None,
        })
    });
}

pub fn with_top_level_poller<F>(f: F)
where
    F: FnOnce(Option<&mut dyn TopLevelPoller>),
{
    POLLER.with(|r| match &mut *r.borrow_mut() {
        Some(t) => f(Some(Box::as_mut(t))),
        None => f(None),
    })
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
        let rw = self.waker().as_raw();

        match WakerWithTopLevelPoller::try_from_raw(rw) {
            Some(w) => Some(w.poller),
            None => None,
        }
    }
}

mod executor {
    use super::{set_top_level_poller, ContextExt, SetWaiterError, TopLevelPoller, WakerWaiter};
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

        set_top_level_poller(Some(poller.clone()));

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

        set_top_level_poller::<MyTopLevelPoller>(None);

        res
    }
}

pub use executor::block_on;

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::Future;
    use std::pin::{pin, Pin};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::task::{Context, Poll, Wake};

    struct NoopWaker;

    impl Wake for NoopWaker {
        fn wake(self: Arc<Self>) {}
    }

    struct MyTopLevelPoller {
        waiter: RefCell<Option<WakerWaiter>>,
    }

    impl TopLevelPoller for Rc<MyTopLevelPoller> {
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
        fn wait(self: Arc<Self>) {}
        fn cancel(self: Arc<Self>) {}
    }

    struct MyFuture {
        waiter: WakerWaiter,
    }

    impl MyFuture {
        fn new() -> Self {
            Self {
                waiter: WakerWaiter::new(Arc::new(NoopWakerWaiter)),
            }
        }
    }

    impl Future for MyFuture {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context) -> Poll<Self::Output> {
            // first, apply WakerWaiter
            // NOTE: alternatively, the lookup/set could happen when MyFuture
            // is constructed (or when some parent owner is constructed)

            // should be cheap: looking up a thread local
            // NOTE: later on, this could become a property of Context
            with_top_level_poller(|p| {
                match p {
                    Some(p) => {
                        // can be cheap: it's up to the setter impl, but this
                        // could just be a pointer comparison
                        if p.set_waiter(&self.waiter).is_err() {
                            panic!("Incompatible WakerWaiter already assigned to thread's TopLevelPoller");
                        }
                    }
                    None => panic!("MyFuture requires thread to provide TopLevelPoller"),
                }
            });

            Poll::Ready(())
        }
    }

    #[test]
    fn test_waiter() {
        let waker = Arc::new(NoopWaker).into();
        let poller = Rc::new(MyTopLevelPoller {
            waiter: RefCell::new(None),
        });

        let mut cx = Context::from_waker(&waker);

        // NOTE: later on, this could become a property of Context
        set_top_level_poller(Some(Rc::clone(&poller)));

        let mut fut = pin!(MyFuture::new());

        loop {
            match fut.as_mut().poll(&mut cx) {
                Poll::Ready(res) => break res,
                Poll::Pending => match &*poller.waiter.borrow() {
                    Some(waiter) => Arc::clone(&waiter.0).wait(),
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
