// Copyright 2018 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of
// this software and associated documentation files (the "Software"), to deal in
// the Software without restriction, including without limitation the rights to
// use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
// the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
// FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS
// OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
// CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.

use bytes::BytesMut;
use error::StreamError;
use frame::Body;
use futures::{prelude::*, stream::{Fuse, Stream as FuturesStream}, sync::mpsc, task::{self, Task}};
use std::{cmp::min, fmt, io, sync::{atomic::{AtomicUsize, Ordering}, Arc}, u32, usize};
use tokio_io::{AsyncRead, AsyncWrite};
use Config;


#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct Id(u32);

impl Id {
    pub(crate) fn new(id: u32) -> Id {
        Id(id)
    }

    pub fn is_server(self) -> bool {
        self.0 % 2 == 0
    }

    pub fn is_client(self) -> bool {
        !self.is_server()
    }

    pub fn is_session(self) -> bool {
        self.0 == 0
    }

    pub fn as_u32(self) -> u32 {
        self.0
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}


#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum State {
    Open,
    SendClosed,
    RecvClosed,
    Closed
}


#[derive(Debug)]
pub(crate) struct Window(AtomicUsize);

impl Window {
    pub(crate) fn new(n: AtomicUsize) -> Window {
        Window(n)
    }

    pub(crate) fn decrement(&self, amount: usize) -> usize {
        loop {
            let prev = self.0.load(Ordering::SeqCst);
            let next = prev.checked_sub(amount).unwrap_or(0);
            if self.0.compare_and_swap(prev, next, Ordering::SeqCst) == prev {
                return next
            }
        }
    }

    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::SeqCst)
    }

    pub(crate) fn set(&self, val: usize) {
        self.0.store(val, Ordering::SeqCst)
    }
}


#[derive(Debug)]
pub(crate) enum Item {
    Data(Body),
    WindowUpdate(u32),
    Reset,
    Finish
}


pub(crate) type Sender = mpsc::UnboundedSender<(Id, Item)>;
pub(crate) type Receiver = Fuse<mpsc::UnboundedReceiver<Item>>;


pub struct Stream {
    id: Id,
    state: State,
    config: Arc<Config>,
    recv_window: Arc<Window>,
    send_window: u32,
    buffer: BytesMut,
    sender: Sender,
    receiver: Receiver,
    writer_tasks: Vec<Task>
}

impl Drop for Stream {
    fn drop(&mut self) {
        let _ = self.sender.unbounded_send((self.id, Item::Reset));
    }
}

impl fmt::Debug for Stream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Stream {{ id: {}, state: {:?} }}", self.id, self.state)
    }
}

impl Stream {
    pub(crate) fn new(id: Id, c: Arc<Config>, tx: Sender, rx: Receiver, rw: Arc<Window>) -> Stream {
        let send_window = c.receive_window;
        Stream {
            id,
            state: State::Open,
            config: c,
            recv_window: rw,
            send_window,
            buffer: BytesMut::new(),
            sender: tx,
            receiver: rx,
            writer_tasks: Vec::new()
        }
    }

    pub fn id(&self) -> Id {
        self.id
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn reset(mut self) -> Result<(), StreamError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        self.send_item(Item::Reset)
    }

    pub fn finish(&mut self) -> Result<(), StreamError> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(StreamError::StreamClosed(self.id))
        }
        self.send_item(Item::Finish)?;
        self.update_state(State::SendClosed);
        Ok(())
    }

    fn update_state(&mut self, next: State) {
        use self::State::*;
        let current = self.state;
        match (current, next) {
            (Closed,              _) => {}
            (Open,                _) => self.state = next,
            (RecvClosed,     Closed) => self.state = Closed,
            (RecvClosed,       Open) => {}
            (RecvClosed, RecvClosed) => {}
            (RecvClosed, SendClosed) => self.state = Closed,
            (SendClosed,     Closed) => self.state = Closed,
            (SendClosed,       Open) => {}
            (SendClosed, RecvClosed) => self.state = Closed,
            (SendClosed, SendClosed) => {}
        }
        trace!("[{}] {:?} -> {:?}", self.id, current, next);
    }

    fn send_item(&mut self, item: Item) -> Result<(), StreamError> {
        if self.sender.unbounded_send((self.id, item)).is_err() {
            self.update_state(State::SendClosed);
            return Err(StreamError::StreamClosed(self.id))
        }
        Ok(())
    }

    fn poll_receiver(&mut self) -> Async<()> {
        loop {
            match self.receiver.poll() {
                Err(()) => {
                    self.update_state(State::RecvClosed);
                    return Async::Ready(())
                }
                Ok(Async::NotReady) => {
                    return Async::NotReady
                }
                Ok(Async::Ready(item)) => match item {
                    Some(Item::Data(body)) => {
                        trace!("[{}] received data: {:?}", self.id, body);
                        let body_len = body.len();
                        self.buffer.extend(body.into_bytes());
                        let remaining = self.recv_window.decrement(body_len);
                        if remaining == 0 {
                            trace!("[{}] received window exhausted", self.id);
                            let item = Item::WindowUpdate(self.config.receive_window);
                            if self.send_item(item).is_err() {
                                error!("[{}] failed to send window update", self.id);
                            }
                            self.recv_window.set(self.config.receive_window as usize);
                        }
                        continue
                    }
                    Some(Item::WindowUpdate(n)) => {
                        trace!("[{}] received window update: {}", self.id, n);
                        self.send_window = self.send_window.saturating_add(n);
                        for task in self.writer_tasks.drain(..) {
                            trace!("[{}] notifying writer task", self.id);
                            task.notify()
                        }
                        continue
                    }
                    Some(Item::Finish) => {
                        trace!("[{}] received finish", self.id);
                        self.update_state(State::SendClosed);
                        continue
                    }
                    Some(Item::Reset) => {
                        trace!("[{}] received reset", self.id);
                        self.update_state(State::Closed);
                        return Async::Ready(())
                    }
                    None => {
                        trace!("[{}] receiver returned None", self.id);
                        self.update_state(State::RecvClosed);
                        return Async::Ready(())
                    }
                }
            }
        }
    }

    fn drain_buffer(&mut self, buf: &mut [u8]) -> usize {
        if self.buffer.is_empty() {
            return 0
        }
        let n = min(buf.len(), self.buffer.len());
        (&mut buf[0..n]).copy_from_slice(&self.buffer.split_to(n));
        n
    }
}

impl io::Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let k = self.drain_buffer(buf);
        if self.state == State::Closed || self.state == State::RecvClosed {
            return Ok(k)
        }
        match self.poll_receiver() {
            Async::NotReady => {
                if k == 0 && self.buffer.is_empty() {
                    return Err(io::Error::new(io::ErrorKind::WouldBlock, "not ready"))
                }
                Ok(k + self.drain_buffer(&mut buf[k..]))
            }
            Async::Ready(()) => {
                Ok(k + self.drain_buffer(&mut buf[k..]))
            }
        }
    }
}

impl AsyncRead for Stream { }

impl io::Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.state == State::Closed || self.state == State::SendClosed {
            return Err(io::Error::new(io::ErrorKind::Other, "stream closed"))
        }

        self.poll_receiver();

        if self.state == State::Closed {
            return Ok(0)
        }

        if self.send_window == 0 {
            trace!("[{}] write: window empty ({} tasks waiting)", self.id, self.writer_tasks.len());
            self.writer_tasks.push(task::current());
            return Err(io::Error::new(io::ErrorKind::WouldBlock, "window empty"))
        }

        let len = min(buf.len(), self.send_window as usize);
        self.send_window -= len as u32;
        let body = Body::from_bytes((&buf[0..len]).into()).expect("buf len <= max body len");

        trace!("[{}] write: {:?}", self.id, body);
        if self.send_item(Item::Data(body)).is_err() {
            Err(io::Error::new(io::ErrorKind::Other, "stream closed"))
        } else {
            Ok(len)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl AsyncWrite for Stream {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        Ok(Async::Ready(()))
    }
}
