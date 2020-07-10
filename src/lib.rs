use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use event_listener::{Event, EventListener};
use intmap::IntMap;
use pin_project_lite::pin_project;
use std::any::Any;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
#[cfg(feature="smol")]
use smol::Task;

// RUNNING -> ENTANGLED_SUCCEEDED -> ENTANGLED_FAILED -> SUCCEEDED -> FAILED
const RUNNING: u8 = 0;
const SUCCEEDED: u8 = 1 << 0;
const FAILED: u8 = 1 << 1;
const ENTANGLED: u8 = 1 << 2;

const ENTANGLED_SUCCEEDED: u8 = ENTANGLED | SUCCEEDED;
const ENTANGLED_FAILED: u8 = ENTANGLED | FAILED;

pin_project! {
    pub struct Particle<F> {
        #[pin]
        fut: F,
        inner: Arc<Inner>,
        entangled: IntMap<Wave>,
    }
}

pub struct Wave {
    inner: Arc<Inner>,
    listener: Option<EventListener>,
}

#[derive(Debug)]
pub enum Error {
    Error(anyhow::Error),
    Panic(Box<dyn Any + Send + 'static>),
    ParticleFailure,
    EntangledFailure,
}

struct Inner {
    status: AtomicU8,
    event: Event,
}


impl<F> Particle<F> {
    pub fn new<T, E>(fut: F) -> Self
    where
        F: Future<Output = Result<T, E>>,
        E: Into<anyhow::Error>,
    {
        Particle {
            fut,
            inner: Arc::new(Inner {
                status: AtomicU8::new(RUNNING),
                event: Event::new(),
            }),
            entangled: IntMap::new(),
        }
    }

    pub fn as_wave(&self) -> Wave {
        Wave {
            inner: self.inner.clone(),
            listener: None,
        }
    }

    pub fn entangle(&mut self, with: Wave) {
        if !Arc::ptr_eq(&self.inner, &with.inner) {
            self.entangled.insert(&*with.inner as *const _ as u64, with);
        }
    }

    #[cfg(debug_assertions)]
    pub fn status(&self) -> u8 {
        self.inner.status.load(Ordering::SeqCst)
    }
}

#[cfg(feature="smol")]
impl<F> Particle<F> {
    pub fn spawn<T, E>(fut: F) -> (Task<Result<Option<T>, Error>>, Wave)
    where
        F: 'static + Future<Output = Result<T, E>> + Send,
        E: Into<anyhow::Error>,
        T: 'static + Send {
        let particle = Particle::new(fut);
        let wave = particle.as_wave();
        (Task::spawn(particle), wave)
    }
    pub fn spawn_blocking<T, E>(fut: F) -> (Task<Result<Option<T>, Error>>, Wave)
    where
        F: 'static + Future<Output = Result<T, E>> + Send,
        E: Into<anyhow::Error>,
        T: 'static + Send {
        let particle = Particle::new(fut);
        let wave = particle.as_wave();
        (Task::blocking(particle), wave)
    }
    pub fn spawn_local<T, E>(fut: F) -> (Task<Result<Option<T>, Error>>, Wave)
    where
        F: 'static + Future<Output = Result<T, E>>,
        E: Into<anyhow::Error>,
        T: 'static {
        let particle = Particle::new(fut);
        let wave = particle.as_wave();
        (Task::local(particle), wave)
    }
}

impl<F> Particle<F> {
    fn succeeded(self: Pin<&mut Self>) {
        self.inner.succeeded();
        self.entangled_succeeded(false);
    }

    fn failed(self: Pin<&mut Self>) {
        self.inner.failed();
        self.entangled_failed(false);
    }

    // Returns `true` if the new status of the particle and its entangled waves is equal to
    // `ENTANGLED_SUCCEEDED` or `SUCCEEDED`, or `false` if it is equal to `ENTANGLED_FAILED` or
    // `FAILED`.
    fn entangled_succeeded(mut self: Pin<&mut Self>, inner: bool) -> bool {
        if inner && !self.inner.entangled_succeeded() {
            self.entangled_failed(false);
            return false;
        }

        let mut failed = false;
        for wave in self.as_mut().project().entangled.values_mut() {
            if !wave.inner.entangled_succeeded() {
                failed = true;
                break;
            }
        }

        if failed {
            self.entangled_failed(false);
            false
        } else {
            self.project().entangled.clear();
            true
        }
    }

    fn entangled_failed(self: Pin<&mut Self>, inner: bool) {
        if inner {
            self.inner.entangled_failed();
        }

        for (_, wave) in self.project().entangled.drain() {
            wave.inner.entangled_failed();
        }
    }
}

impl Inner {
    fn succeeded(&self) {
        let mut status = RUNNING;
        loop {
            // [...] -> SUCCEEDED -> FAILED
            match self.status.compare_exchange_weak(status, SUCCEEDED, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(RUNNING) => {
                    self.event.notify(!0);
                    break;
                }
                Ok(_) | Err(FAILED) | Err(SUCCEEDED) => break,
                Err(cur) => status = cur,
            }
        }
    }

    fn failed(&self) {
        // [...] -> FAILED
        if self.status.swap(FAILED, Ordering::SeqCst) == RUNNING {
            self.event.notify(!0);
        }
    }

    // Returns `true` if the new status is equal to `ENTANGLED_SUCCEEDED` or `SUCCEEDED`, or `false`
    // if it is equal to `ENTANGLED_FAILED` or `FAILED`.
    fn entangled_succeeded(&self) -> bool {
        // RUNNING -> ENTANGLED_SUCCEEDED -> [...]
        match self.status.compare_and_swap(RUNNING, ENTANGLED_SUCCEEDED, Ordering::SeqCst) {
            RUNNING => {
                self.event.notify(!0);
                true
            }
            FAILED | ENTANGLED_FAILED => false,
            _ => true,
        }
    }

    fn entangled_failed(&self) {
        let mut status = RUNNING;
        loop {
            // [...] -> ENTANGLED_FAILED -> SUCCEEDED -> FAILED
            match self.status.compare_exchange_weak(status, ENTANGLED_FAILED, Ordering::SeqCst, Ordering::SeqCst) {
                Ok(RUNNING) => {
                    self.event.notify(!0);
                    break;
                }
                Ok(_) | Err(ENTANGLED_FAILED) | Err(SUCCEEDED) | Err(FAILED) => break,
                Err(cur) => status = cur,
            }
        }
    }
}

impl<F, T, E> Future for Particle<F>
where
    F: Future<Output = Result<T, E>>,
    E: Into<anyhow::Error>,
{
    type Output = Result<Option<T>, Error>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let this = self.as_mut().project();

        // Check the particle's current state and eventually propagate it to its entangled waves.
        match this.inner.status.load(Ordering::SeqCst) {
            SUCCEEDED | ENTANGLED_SUCCEEDED => return Poll::Ready(Ok(None)),
            FAILED => {
                self.entangled_failed(false);
                return Poll::Ready(Err(Error::ParticleFailure));
            }
            ENTANGLED_FAILED => {
                self.entangled_failed(false);
                return Poll::Ready(Err(Error::EntangledFailure));
            }
            _ => (),
        }

        // Check for entangled waves whose status is `SUCCEEDED` or `ENTANGLED_SUCCEEDED` and stop
        // when finding one whose status is `FAILED` or `ENTANGLED_FAILED`.
        let mut status = RUNNING;
        for mut wave in this.entangled.values_mut() {
            match Pin::new(&mut wave).poll(ctx) {
                Poll::Ready(Ok(())) => status = SUCCEEDED,
                Poll::Ready(Err(_)) => {
                    status = FAILED;
                    break;
                }
                Poll::Pending => (),
            }
        }

        // If at least one entangled wave's status wasn't `RUNNING`, update the status of the
        // particle and all its entangled waves.
        match status {
            SUCCEEDED => {
                if self.entangled_succeeded(true) {
                    return Poll::Ready(Ok(None));
                } else {
                    return Poll::Ready(Err(Error::EntangledFailure));
                }
            },
            FAILED => {
                self.entangled_failed(true);
                return Poll::Ready(Err(Error::EntangledFailure));
            }
            _ => (),
        }

        // Otherwise, poll the inner future and eventually update statuses.
        match catch_unwind(AssertUnwindSafe(|| this.fut.poll(ctx))) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(Ok(ok))) => {
                self.succeeded();
                Poll::Ready(Ok(Some(ok)))
            }
            Ok(Poll::Ready(Err(err))) => {
                self.failed();
                Poll::Ready(Err(Error::Error(err.into())))
            }
            Err(err) => {
                self.failed();
                Poll::Ready(Err(Error::Panic(err)))
            }
        }
    }
}

impl Future for Wave {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, ctx: &mut Context) -> Poll<Self::Output> {
        let mut listener = if let Some(listener) = self.listener.take() {
            listener
        } else {
            // Create a new `EventListener` first to be sure that the wave's status isn't updated
            // between the time we first check it and the time we finish creating the listener.
            let listener = self.inner.event.listen();
            match self.inner.status.load(Ordering::SeqCst) {
                SUCCEEDED | ENTANGLED_SUCCEEDED => return Poll::Ready(Ok(())),
                FAILED => return Poll::Ready(Err(Error::ParticleFailure)),
                ENTANGLED_FAILED => return Poll::Ready(Err(Error::EntangledFailure)),
                _ => listener,
            }
        };

        if Pin::new(&mut listener).poll(ctx).is_ready() {
            match self.inner.status.load(Ordering::SeqCst) {
                SUCCEEDED | ENTANGLED_SUCCEEDED => Poll::Ready(Ok(())),
                FAILED => Poll::Ready(Err(Error::ParticleFailure)),
                ENTANGLED_FAILED => Poll::Ready(Err(Error::EntangledFailure)),
                _ => unreachable!(),
            }
        } else {
            self.listener = Some(listener);
            Poll::Pending
        }
    }
}

impl Clone for Wave {
    fn clone(&self) -> Self {
        Wave {
            inner: self.inner.clone(),
            listener: None,
        }
    }
}
