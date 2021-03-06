// Copyright (c) 2016 Sandstorm Development Group, Inc. and contributors
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.

use std::io;
use std::collections::VecDeque;
use std::rc::Rc;
use std::cell::RefCell;
use futures::{self, task, Async, Future, Poll, Complete, Oneshot};

use capnp::{Error};

use serialize::{self, AsOutputSegments};


enum State<W, M> where W: io::Write, M: AsOutputSegments {
    Writing(serialize::Write<W, M>, Complete<M>),
    BetweenWrites(W),
    Empty,
}

/// A queue of messages being written.
pub struct WriteQueue<W, M> where W: io::Write, M: AsOutputSegments {
    inner: Rc<RefCell<Inner<M>>>,
    state: State<W, M>,
}

struct Inner<M> {
    queue: VecDeque<(M, Complete<M>)>,
    sender_count: usize,
    task: Option<task::Task>,
}

/// A handle that allows message to be sent to a `WriteQueue`.
pub struct Sender<M> where M: AsOutputSegments {
    inner: Rc<RefCell<Inner<M>>>,
}

impl <M> Clone for Sender<M> where M: AsOutputSegments {
    fn clone(&self) -> Sender<M> {
        self.inner.borrow_mut().sender_count += 1;
        Sender { inner: self.inner.clone() }
    }
}

impl <M> Drop for Sender<M> where M: AsOutputSegments {
    fn drop(&mut self) {
        self.inner.borrow_mut().sender_count -= 1;
    }
}

pub fn write_queue<W, M>(writer: W) -> (Sender<M>, WriteQueue<W, M>)
    where W: io::Write, M: AsOutputSegments
{
    let inner = Rc::new(RefCell::new(Inner {
        queue: VecDeque::new(),
        task: None,
        sender_count: 1,
    }));

    let queue = WriteQueue {
        inner: inner.clone(),
        state: State::BetweenWrites(writer),
    };

    let sender = Sender { inner: inner };

    (sender, queue)
}

impl <M> Sender<M> where M: AsOutputSegments {
    /// Enqueues a message to be written.
    pub fn send(&mut self, message: M) -> Oneshot<M> {
        let (complete, oneshot) = futures::oneshot();
        self.inner.borrow_mut().queue.push_back((message, complete));

        match self.inner.borrow_mut().task.take() {
            Some(t) => t.unpark(),
            None => (),
        }

        oneshot
    }

    /// Returns the number of messages queued to be written, not including any in-progress write.
    pub fn len(&mut self) -> usize {
        self.inner.borrow().queue.len()
    }
}

enum IntermediateState<W, M> where W: io::Write, M: AsOutputSegments {
    WriteDone(M, W),
    StartWrite(M, Complete<M>),
    Resolve,
}

impl <W, M> Future for WriteQueue<W, M> where W: io::Write, M: AsOutputSegments {
    type Item = W; // Resolves when all senders have been dropped and all messages written.
    type Error = Error;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            let next = match self.state {
                State::Writing(ref mut write, ref mut _complete) => {
                    let (w, m) = try_ready!(Future::poll(write));
                    IntermediateState::WriteDone(m, w)
                }
                State::BetweenWrites(ref mut _writer) => {
                    let front = self.inner.borrow_mut().queue.pop_front();
                    match front {
                        Some((m, complete)) => {
                            IntermediateState::StartWrite(m, complete)
                        }
                        None => {
                            let count = self.inner.borrow().sender_count;
                            if count == 0 {
                                IntermediateState::Resolve
                            } else {
                                self.inner.borrow_mut().task = Some(task::park());
                                return Ok(Async::NotReady)
                            }
                        }
                    }
                }
                State::Empty => unreachable!(),
            };

            match next {
                IntermediateState::WriteDone(m, w) => {
                    match ::std::mem::replace(&mut self.state, State::BetweenWrites(w)) {
                        State::Writing(_, complete) => {
                            complete.complete(m)
                        }
                        _ => unreachable!(),
                    }
                }
                IntermediateState::StartWrite(m, c) => {
                    let new_state = match ::std::mem::replace(&mut self.state, State::Empty) {
                        State::BetweenWrites(w) => {
                            State::Writing(::serialize::write_message(w, m), c)
                        }
                        _ => unreachable!(),
                    };
                    self.state = new_state;
                }
                IntermediateState::Resolve => {
                    match ::std::mem::replace(&mut self.state, State::Empty) {
                        State::BetweenWrites(w) => {
                            return Ok(Async::Ready(w))
                        }
                        _ => unreachable!(),
                    }
                }
            }
        }
    }
}
