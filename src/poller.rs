use futures::{Future};
use futures::sync::oneshot;

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use stack::Stack;

enum Slot<T> {
    Next(usize),
    Data(T),
}

struct Inner<T, E> {
    // A slab of futures that are being executed. Each slot in this vector is
    // either an active future or a pointer to the next empty slot. This is used
    // to get O(1) deallocation in the slab and O(1) allocation.
    //
    // The `next_future` field is the next slot in the `futures` array that's a
    // `Slot::Next` variant. If it points to the end of the array then the array
    // is full.
    futures: RefCell<Vec<Slot<Box<Future<Item=T, Error=E>>>>>,
    next_future: Cell<usize>,

    new_futures: RefCell<Vec<Box<Future<Item=T, Error=E>>>>,

    stack: Arc<Stack<usize>>,
    reaper: RefCell<Box<Finisher<T, E>>>,

    terminate_with: RefCell<Option<Result<(), E>>>,

    handle_count: Cell<usize>,
    task: RefCell<Option<::futures::task::Task>>,
}


#[must_use = "a TaskSet does nothing unless polled"]
pub struct Poller<T, E> {
    inner: Rc<Inner<T, E>>,
}

impl<T, E> Poller<T, E> where T: 'static, E: 'static {
    pub fn new(reaper: Box<Finisher<T, E>>)
               -> (PollerHandle<T, E>, Poller<T, E>)
        where E: 'static, T: 'static, E: ::std::fmt::Debug,
    {
        let inner = Rc::new(Inner {
            futures: RefCell::new(Vec::new()),
            next_future: Cell::new(0),
            new_futures: RefCell::new(Vec::new()),
            stack: Arc::new(Stack::new()),
            reaper: RefCell::new(reaper),
            terminate_with: RefCell::new(None),
            handle_count: Cell::new(1),
            task: RefCell::new(None),
        });

        let weak_inner = Rc::downgrade(&inner);

        let set = Poller {
            inner: inner,
        };

        let handle = PollerHandle {
            inner: weak_inner,
        };

        (handle, set)
    }
}

pub struct PollerHandle<T, E> {
    inner: Weak<Inner<T, E>>,
}

impl<T, E> Clone for PollerHandle<T, E> {
    fn clone(&self) -> PollerHandle<T, E> {
        match self.inner.upgrade() {
            None => (),
            Some(inner) => {
                inner.handle_count.set(inner.handle_count.get() + 1);
            }
        }
        PollerHandle {
            inner: self.inner.clone()
        }
    }
}

impl <T, E> Drop for PollerHandle<T, E> {
    fn drop(&mut self) {
        match self.inner.upgrade() {
            None => (),
            Some(inner) => {
                inner.handle_count.set(inner.handle_count.get() - 1);
            }
        }
    }
}

impl <T, E> PollerHandle<T, E> where T: 'static, E: 'static {
    pub fn add<F>(&mut self, promise: F)
        where F: Future<Item=T, Error=E> + 'static
    {
        match self.inner.upgrade() {
            None => (),
            Some(rc_inner) => {
                rc_inner.new_futures.borrow_mut().push(Box::new(promise));

                match rc_inner.task.borrow_mut().take() {
                    Some(t) => t.unpark(),
                    None => (),
                }
            }
        }
    }

    pub fn terminate(&mut self, result: Result<(), E>) {
        match self.inner.upgrade() {
            None => (),
            Some(rc_inner) => {
                *rc_inner.terminate_with.borrow_mut() = Some(result);

                match rc_inner.task.borrow_mut().take() {
                    Some(t) => t.unpark(),
                    None => (),
                }
            }
        }
    }
}


impl <E> PollerHandle<(), E> where E: 'static {
    // Transform a future into a new future that gets executed even if it is never polled.
    // Dropping the returned future cancels the computation.
    pub fn add_cancelable<T, F>(&mut self, f: F) -> Box<Future<Item=Result<T, E>, Error=oneshot::Canceled>>
        where F: Future<Item=T, Error=E> + 'static,
              T: 'static
    {
        let (tx, rx) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel::<()>();
        self.add(
            f.then(move |r| {tx.complete(r); Ok(())}).select(rx2.then(|_| Ok(())))
                .map_err(|e| e.0).map(|_| ()));
        Box::new(rx.then(|v| { tx2.complete(()); v}))
    }
}

pub trait Finisher<T, E> where T: 'static, E: 'static
{
    fn task_succeeded(&mut self, _value: T) {}
    fn task_failed(&mut self, error: E);
}

impl <T, E> Future for Poller<T, E> where T: 'static, E: 'static {
    type Item = ();
    type Error = E;

    fn poll(&mut self) -> ::futures::Poll<Self::Item, Self::Error> {
        match self.inner.terminate_with.borrow_mut().take() {
            None => (),
            Some(Ok(v)) => return Ok(::futures::Async::Ready(v)),
            Some(Err(e)) => return Err(e),
        }

        let new_futures = ::std::mem::replace(&mut *self.inner.new_futures.borrow_mut(), Vec::new());
        for future in new_futures {
            let added_idx = self.inner.next_future.get();
            if self.inner.next_future.get() == self.inner.futures.borrow().len() {
                self.inner.futures.borrow_mut().push(Slot::Data(future));
                self.inner.next_future.set(self.inner.next_future.get() + 1);
            } else {
                match ::std::mem::replace(&mut self.inner.futures.borrow_mut()[self.inner.next_future.get()],
                                          Slot::Data(future)) {
                    Slot::Next(next) => self.inner.next_future.set(next),
                    Slot::Data(_) => unreachable!(),
                }
            }

            self.inner.stack.push(added_idx);
        }

        let drain = self.inner.stack.drain();
        for idx in drain {
            match self.inner.futures.borrow_mut()[idx] {
                Slot::Next(_) => continue,
                Slot::Data(ref mut f) => {
                    let event = ::futures::task::UnparkEvent::new(self.inner.stack.clone(), idx);
                    match ::futures::task::with_unpark_event(event, || f.poll()) {
                        Ok(::futures::Async::NotReady) => continue,
                        Ok(::futures::Async::Ready(v)) => {
                            self.inner.reaper.borrow_mut().task_succeeded(v);
                        }
                        Err(e) => {
                            self.inner.reaper.borrow_mut().task_failed(e);
                        }
                    }
                }
            }
            self.inner.futures.borrow_mut()[idx] = Slot::Next(self.inner.next_future.get());
            self.inner.next_future.set(idx);
        }

        if self.inner.futures.borrow().len() == 0 && self.inner.handle_count.get() == 0 {
            Ok(::futures::Async::Ready(()))
        } else {
            let task = ::futures::task::park();
            if self.inner.new_futures.borrow().len() > 0 {
                // Some new futures got added when we called poll().
                task.unpark();
            }
            *self.inner.task.borrow_mut() = Some(task);
            Ok(::futures::Async::NotReady)
        }
    }
}
