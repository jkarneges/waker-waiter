mod tokio_integration {
    use std::future::Future;
    use std::io;
    use std::pin::Pin;
    use std::sync::{Arc, Mutex, Weak};
    use std::task::{Context, Poll, Waker};
    use tokio::runtime;
    use waker_waiter::{
        ContextExt, WakerWait, WakerWaiter, WakerWaiterCancel, WakerWaiterCanceler,
    };

    static WAITER_MANAGER: Mutex<Option<Arc<WaiterManager>>> = Mutex::new(None);

    struct WaiterManager {
        // tokio runtime that we will associate I/O objects with
        runtime: runtime::Runtime,

        // a pre-constructed value that we can return by reference
        waiter: WakerWaiter,

        // for completing PendingOnce
        waker: Mutex<Option<Waker>>,
    }

    impl WaiterManager {
        fn current() -> Arc<Self> {
            let mut manager = WAITER_MANAGER.lock().unwrap();

            if manager.is_none() {
                // construct a single-threaded runtime and set up an unpark
                // handler. we assume when block_on() is used with tokio's
                // single-threaded runtime that the thread parks whenever it
                // begins waiting for events and unparks when events have
                // occurred. in that case, we can use the unpark callback as
                // and indication that events have occurred
                let runtime = runtime::Builder::new_current_thread()
                    .enable_all()
                    .on_thread_unpark(|| {
                        println!("thread unparking");

                        // tell PendingOnce to complete
                        Self::current().wake();
                    })
                    .build()
                    .unwrap();

                *manager = Some(Arc::new_cyclic(|m| {
                    let waiter = Arc::new(Waiter(m.clone())).into();

                    Self {
                        runtime,
                        waiter,
                        waker: Mutex::new(None),
                    }
                }));
            }

            Arc::clone(manager.as_ref().unwrap())
        }

        fn waiter<'a>(self: &'a Arc<Self>) -> &'a WakerWaiter {
            &self.waiter
        }

        fn set_waker(&self, waker: &Waker) {
            *self.waker.lock().unwrap() = Some(waker.clone());
        }

        fn clear_waker(&self) {
            *self.waker.lock().unwrap() = None;
        }

        fn wake(&self) {
            if let Some(waker) = self.waker.lock().unwrap().take() {
                waker.wake();
            }
        }
    }

    struct Waiter(Weak<WaiterManager>);

    impl WakerWait for Waiter {
        fn wait(self: &Arc<Self>) {
            println!("wait start");

            let manager = self.0.upgrade().unwrap();

            // tell the runtime to run a single task that returns pending, in
            // order to cause the runtime to park and wait for events. we
            // assume the runtime will unpark once any events are received,
            // even if they are for I/O objects that are not living in any
            // tokio-managed tasks
            manager.runtime.block_on(PendingOnce::new(&manager));

            println!("wait end");
        }

        fn canceler(self: &Arc<Self>) -> WakerWaiterCanceler {
            Arc::clone(self).into()
        }
    }

    impl WakerWaiterCancel for Waiter {
        fn cancel(self: &Arc<Self>) {
            if let Some(manager) = self.0.upgrade() {
                // tell PendingOnce to complete
                manager.wake();
            }
        }
    }

    // a future that returns Pending on the first call to poll, and Ready on
    // the second call
    struct PendingOnce<'a> {
        done: bool,
        manager: &'a WaiterManager,
    }

    impl<'a> PendingOnce<'a> {
        fn new(manager: &'a WaiterManager) -> Self {
            Self {
                done: false,
                manager,
            }
        }
    }

    impl<'a> Future for PendingOnce<'a> {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let s = &mut *self;

            if s.done {
                Poll::Ready(())
            } else {
                s.done = true;
                s.manager.set_waker(cx.waker());

                Poll::Pending
            }
        }
    }

    impl Drop for PendingOnce<'_> {
        fn drop(&mut self) {
            self.manager.clear_waker();
        }
    }

    struct EnsureRegistered;

    impl Future for EnsureRegistered {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
            let p = match cx.top_level_poller() {
                Some(p) => p,
                None => panic!("Thread does not provide TopLevelPoller"),
            };

            if p.set_waiter(WaiterManager::current().waiter()).is_err() {
                panic!("Incompatible waiter already assigned to TopLevelPoller");
            }

            Poll::Ready(())
        }
    }

    pub struct TcpListener(tokio::net::TcpListener);

    impl TcpListener {
        pub async fn bind<A: tokio::net::ToSocketAddrs>(addr: A) -> Result<Self, io::Error> {
            EnsureRegistered.await;

            let l = {
                // associate object with our tokio runtime, even though the
                // object does not live in a tokio-managed task
                let _guard = WaiterManager::current().runtime.enter();
                tokio::net::TcpListener::bind(addr).await?
            };

            Ok(Self(l))
        }

        pub fn local_addr(&self) -> Result<std::net::SocketAddr, io::Error> {
            self.0.local_addr()
        }

        pub async fn accept(
            &self,
        ) -> Result<(tokio::net::TcpStream, std::net::SocketAddr), io::Error> {
            EnsureRegistered.await;

            let s = {
                // associate object with our tokio runtime, even though the
                // object does not live in a tokio-managed task
                let _guard = WaiterManager::current().runtime.enter();
                self.0.accept().await?
            };

            Ok(s)
        }
    }

    pub struct TcpStream;

    impl TcpStream {
        pub async fn connect<A: tokio::net::ToSocketAddrs>(
            addr: A,
        ) -> Result<tokio::net::TcpStream, io::Error> {
            EnsureRegistered.await;

            let s = {
                // associate object with our tokio runtime, even though the
                // object does not live in a tokio-managed task
                let _guard = WaiterManager::current().runtime.enter();
                tokio::net::TcpStream::connect(addr).await?
            };

            Ok(s)
        }
    }
}

use std::error::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_integration::{TcpListener, TcpStream};

async fn io_test() -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;

    let mut client = TcpStream::connect(listener.local_addr()?).await?;

    let (mut server, _) = listener.accept().await?;

    client.write(b"hello").await?;

    let mut buf = [0; 1024];
    let size = server.read(&mut buf).await?;
    let buf = &buf[..size];

    assert_eq!(buf, b"hello");

    Ok(())
}

fn main() {
    env_logger::init();

    waker_waiter::block_on(async {
        io_test().await.unwrap();
    });
}
