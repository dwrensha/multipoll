use std::pin::Pin;
use std::task::{Context, Poll};
use futures::{Future, FutureExt, Stream};
use futures::stream::FuturesUnordered;
use futures::channel::mpsc;

use std::cell::{RefCell};
use std::rc::Rc;

enum EnqueuedTask<E> {
    Task(Pin<Box<dyn Future<Output=Result<(), E>>>>),
    Terminate(Result<(), E>),
}

enum TaskInProgress<E> {
    Task(Pin<Box<dyn Future<Output=()>>>),
    Terminate(Option<Result<(), E>>),
}

impl <E> Unpin for TaskInProgress<E> {}

enum TaskDone<E> {
    Continue,
    Terminate(Result<(), E>),
}

impl <E> Future for TaskInProgress<E> {
    type Output = TaskDone<E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        match *self {
            TaskInProgress::Terminate(ref mut r) =>
                Poll::Ready(TaskDone::Terminate(r.take().unwrap())),
            TaskInProgress::Task(ref mut f) => {
                match f.as_mut().poll(cx) {
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(()) => Poll::Ready(TaskDone::Continue),
                }
            }

        }
    }
}

#[must_use = "a Poller does nothing unless polled"]
pub struct Poller<E> {
    enqueued: Option<mpsc::UnboundedReceiver<EnqueuedTask<E>>>,
    in_progress: FuturesUnordered<TaskInProgress<E>>,
    reaper: Rc<RefCell<Box<dyn Finisher<E>>>>,
}

impl<E> Poller<E> where E: 'static {
    pub fn new(reaper: Box<dyn Finisher<E>>)
               -> (PollerHandle<E>, Poller<E>)
        where E: 'static, E: ::std::fmt::Debug,
    {
        let (sender, receiver) = mpsc::unbounded();

        let set = Poller {
            enqueued: Some(receiver),
            in_progress: FuturesUnordered::new(),
            reaper: Rc::new(RefCell::new(reaper)),
        };

        // If the FuturesUnordered ever gets empty, its stream will terminate, which
        // is not what we want. So we make sure there is always at least one future in it.
        set.in_progress.push(TaskInProgress::Task(Box::pin(::futures::future::pending())));

        let handle = PollerHandle {
            sender: sender,
        };

        (handle, set)
    }
}

#[derive(Clone)]
pub struct PollerHandle<E> {
    sender: mpsc::UnboundedSender<EnqueuedTask<E>>
}

impl <E> PollerHandle<E> where E: 'static {
    pub fn add<F>(&mut self, f: F)
        where F: Future<Output = Result<(), E>> + 'static
    {
        let _ = self.sender.unbounded_send(EnqueuedTask::Task(Box::pin(f)));
    }

    pub fn terminate(&mut self, result: Result<(), E>) {
        let _ = self.sender.unbounded_send(EnqueuedTask::Terminate(result));
    }
}

pub trait Finisher<E> where E: 'static
{
    fn task_succeeded(&mut self) {}
    fn task_failed(&mut self, error: E);
}

impl <E> Future for Poller<E> where E: 'static {
    type Output = Result<(),E>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        let mut enqueued_stream_complete = false;
        if let Poller { enqueued: Some(ref mut enqueued), ref mut in_progress, ref reaper, ..} = self.as_mut().get_mut() {
            loop {
                match Pin::new(&mut *enqueued).poll_next(cx) {
                    Poll::Pending => break,
                    Poll::Ready(None) => {
                        enqueued_stream_complete = true;
                        break;
                    }
                    Poll::Ready(Some(EnqueuedTask::Terminate(r))) => {
                        in_progress.push(TaskInProgress::Terminate(Some(r)));
                    }
                    Poll::Ready(Some(EnqueuedTask::Task(f))) => {
                        let reaper = Rc::downgrade(&reaper);
                        in_progress.push(
                            TaskInProgress::Task(Box::pin(
                                f.map(move |r| {
                                    match reaper.upgrade() {
                                        None => (), // Poller must have been dropped.
                                        Some(rc_reaper) => {
                                            match r {
                                                Ok(()) => rc_reaper.borrow_mut().task_succeeded(),
                                                Err(e) => rc_reaper.borrow_mut().task_failed(e),
                                            }
                                        }
                                    }
                                }))));
                    }
                }
            }
        }
        if enqueued_stream_complete {
            drop(self.enqueued.take());
        }

        loop {
            match Stream::poll_next(Pin::new(&mut self.in_progress), cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(v) => {
                    match v {
                        None => return Poll::Ready(Ok(())),
                        Some(TaskDone::Continue) => (),
                        Some(TaskDone::Terminate(Ok(()))) =>
                            return Poll::Ready(Ok(())),
                        Some(TaskDone::Terminate(Err(e))) => return Poll::Ready(Err(e)),
                    }
                }
            }
        }
    }
}
