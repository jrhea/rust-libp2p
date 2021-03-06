// Copyright 2017 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! Contains the `Listener` wrapper, which allows raw communications with a dialer.

use super::*;

use bytes::{Bytes, BytesMut};
use crate::length_delimited::LengthDelimited;
use crate::protocol::{Request, Response, MultistreamSelectError};
use futures::{prelude::*, sink, stream::StreamFuture};
use log::{debug, trace};
use std::{marker, mem};
use tokio_io::{AsyncRead, AsyncWrite};

/// Wraps around a `AsyncRead+AsyncWrite`. Assumes that we're on the listener's side. Produces and
/// accepts messages.
pub struct Listener<R, N> {
    inner: LengthDelimited<R>,
    _protocol_name: marker::PhantomData<N>,
}

impl<R, N> Listener<R, N>
where
    R: AsyncRead + AsyncWrite,
    N: AsRef<[u8]>
{
    /// Takes ownership of a socket and starts the handshake. If the handshake succeeds, the
    /// future returns a `Listener`.
    pub fn listen(inner: R) -> ListenerFuture<R, N> {
        let inner = LengthDelimited::new(inner);
        ListenerFuture {
            inner: ListenerFutureState::Await { inner: inner.into_future() },
            _protocol_name: marker::PhantomData,
        }
    }

    /// Grants back the socket. Typically used after a `ProtocolRequest` has been received and a
    /// `ProtocolAck` has been sent back.
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<R, N> Sink for Listener<R, N>
where
    R: AsyncRead + AsyncWrite,
    N: AsRef<[u8]>
{
    type SinkItem = Response<N>;
    type SinkError = MultistreamSelectError;

    fn start_send(&mut self, response: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        let mut msg = BytesMut::new();
        response.encode(&mut msg)?;
        match self.inner.start_send(msg.freeze())? {
            AsyncSink::NotReady(_) => Ok(AsyncSink::NotReady(response)),
            AsyncSink::Ready => Ok(AsyncSink::Ready)
        }
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(self.inner.poll_complete()?)
    }

    fn close(&mut self) -> Poll<(), Self::SinkError> {
        Ok(self.inner.close()?)
    }
}

impl<R, N> Stream for Listener<R, N>
where
    R: AsyncRead + AsyncWrite,
{
    type Item = Request<Bytes>;
    type Error = MultistreamSelectError;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut msg = match self.inner.poll() {
            Ok(Async::Ready(Some(msg))) => msg,
            Ok(Async::Ready(None)) => return Ok(Async::Ready(None)),
            Ok(Async::NotReady) => return Ok(Async::NotReady),
            Err(err) => return Err(err.into()),
        };

        if msg.get(0) == Some(&b'/') && msg.last() == Some(&b'\n') {
            let len = msg.len();
            let name = msg.split_to(len - 1);
            Ok(Async::Ready(Some(
                Request::Protocol { name },
            )))
        } else if msg == MSG_LS {
            Ok(Async::Ready(Some(
                Request::ListProtocols,
            )))
        } else {
            Err(MultistreamSelectError::UnknownMessage)
        }
    }
}


/// Future, returned by `Listener::new` which performs the handshake and returns
/// the `Listener` if successful.
pub struct ListenerFuture<T: AsyncRead + AsyncWrite, N> {
    inner: ListenerFutureState<T>,
    _protocol_name: marker::PhantomData<N>,
}

enum ListenerFutureState<T: AsyncRead + AsyncWrite> {
    Await {
        inner: StreamFuture<LengthDelimited<T>>
    },
    Reply {
        sender: sink::Send<LengthDelimited<T>>
    },
    Undefined
}

impl<T: AsyncRead + AsyncWrite, N: AsRef<[u8]>> Future for ListenerFuture<T, N> {
    type Item = Listener<T, N>;
    type Error = MultistreamSelectError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        loop {
            match mem::replace(&mut self.inner, ListenerFutureState::Undefined) {
                ListenerFutureState::Await { mut inner } => {
                    let (msg, socket) =
                        match inner.poll() {
                            Ok(Async::Ready(x)) => x,
                            Ok(Async::NotReady) => {
                                self.inner = ListenerFutureState::Await { inner };
                                return Ok(Async::NotReady)
                            }
                            Err((e, _)) => return Err(MultistreamSelectError::from(e))
                        };
                    if msg.as_ref().map(|b| &b[..]) != Some(MSG_MULTISTREAM_1_0) {
                        debug!("Unexpected message: {:?}", msg);
                        return Err(MultistreamSelectError::FailedHandshake)
                    }
                    trace!("sending back /multistream/<version> to finish the handshake");
                    let mut frame = BytesMut::new();
                    Header::Multistream10.encode(&mut frame);
                    let sender = socket.send(frame.freeze());
                    self.inner = ListenerFutureState::Reply { sender }
                }
                ListenerFutureState::Reply { mut sender } => {
                    let listener = match sender.poll()? {
                        Async::Ready(x) => x,
                        Async::NotReady => {
                            self.inner = ListenerFutureState::Reply { sender };
                            return Ok(Async::NotReady)
                        }
                    };
                    return Ok(Async::Ready(Listener {
                        inner: listener,
                        _protocol_name: marker::PhantomData
                    }))
                }
                ListenerFutureState::Undefined =>
                    panic!("ListenerFutureState::poll called after completion")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::runtime::current_thread::Runtime;
    use tokio_tcp::{TcpListener, TcpStream};
    use bytes::Bytes;
    use futures::Future;
    use futures::{Sink, Stream};

    #[test]
    fn wrong_proto_name() {
        let listener = TcpListener::bind(&"127.0.0.1:0".parse().unwrap()).unwrap();
        let listener_addr = listener.local_addr().unwrap();

        let server = listener
            .incoming()
            .into_future()
            .map_err(|(e, _)| e.into())
            .and_then(move |(connec, _)| Listener::listen(connec.unwrap()))
            .and_then(|listener| {
                let name = Bytes::from("invalid-proto");
                listener.send(Response::Protocol { name })
            });

        let client = TcpStream::connect(&listener_addr)
            .from_err()
            .and_then(move |stream| Dialer::<_, Bytes>::dial(stream));

        let mut rt = Runtime::new().unwrap();
        match rt.block_on(server.join(client)) {
            Err(MultistreamSelectError::InvalidProtocolName) => (),
            _ => panic!(),
        }
    }
}
