//! A lock-free stack which supports concurrent pushes and a concurrent call to
//! drain the entire stack all at once.

// Borrowed directly from futures-rs.

use std::prelude::v1::*;

use std::sync::atomic::AtomicUsize;
use std::mem;
use std::marker;
use std::sync::atomic::Ordering::SeqCst;

pub struct Stack<T> {
    head: AtomicUsize,
    _marker: marker::PhantomData<T>,
}

struct Node<T> {
    data: T,
    next: *mut Node<T>,
}

pub struct Drain<T> {
    head: *mut Node<T>,
}

impl<T> Stack<T> {
    pub fn new() -> Stack<T> {
        Stack {
            head: AtomicUsize::new(0),
            _marker: marker::PhantomData,
        }
    }

    pub fn push(&self, data: T) {
        let mut node = Box::new(Node { data: data, next: 0 as *mut _ });
        let mut head = self.head.load(SeqCst);
        loop {
            node.next = head as *mut _;
            let ptr = &*node as *const Node<T> as usize;
            match self.head.compare_exchange(head, ptr, SeqCst, SeqCst) {
                Ok(_) => {
                    mem::forget(node);
                    return
                }
                Err(cur) => head = cur,
            }
        }
    }

    pub fn drain(&self) -> Drain<T> {
        Drain {
            head: self.head.swap(0, SeqCst) as *mut _,
        }
    }
}

impl<T> Drop for Stack<T> {
    fn drop(&mut self) {
        self.drain();
    }
}

impl<T> Iterator for Drain<T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        if self.head.is_null() {
            return None
        }
        unsafe {
            let node = Box::from_raw(self.head);
            self.head = node.next;
            return Some(node.data)
        }
    }
}

impl<T> Drop for Drain<T> {
    fn drop(&mut self) {
        for item in self.by_ref() {
            drop(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::prelude::v1::*;
    use std::rc::Rc;
    use std::cell::Cell;

    use super::Stack;

    struct Set(Rc<Cell<usize>>, usize);

    impl Drop for Set {
        fn drop(&mut self) {
            self.0.set(self.1);
        }
    }

    #[test]
    fn simple() {
        let s = Stack::new();
        s.push(1);
        s.push(2);
        s.push(4);
        assert_eq!(s.drain().collect::<Vec<_>>(), vec![4, 2, 1]);
        s.push(5);
        assert_eq!(s.drain().collect::<Vec<_>>(), vec![5]);
        assert_eq!(s.drain().collect::<Vec<_>>(), vec![]);
    }

    #[test]
    fn drain_drops() {
        let data = Rc::new(Cell::new(0));
        let s = Stack::new();
        s.push(Set(data.clone(), 1));
        drop(s.drain());
        assert_eq!(data.get(), 1);
    }

    #[test]
    fn drop_drops() {
        let data = Rc::new(Cell::new(0));
        let s = Stack::new();
        s.push(Set(data.clone(), 1));
        drop(s);
        assert_eq!(data.get(), 1);
    }
}

impl ::futures::task::EventSet for Stack<usize> {
    fn insert(&self, id: usize) {
        self.push(id);
    }
}
