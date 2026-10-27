//! Bounded byte-stream requests between background work and a socket owner.
//! The socket and its non-Send network stack never cross execution contexts.
use embassy_sync::{blocking_mutex::raw::RawMutex, channel::Channel, signal::Signal};
use embedded_io_async::{Error, ErrorKind, ErrorType, Read, Write};
use heapless::Vec;

/// Firmware transfer bound: keep critical sections and task storage small.
pub const CHUNK_BYTES: usize = 256;

enum Operation<const N: usize> {
    Read(usize),
    Write(Vec<u8, N>),
    Flush,
}

struct Request<const N: usize> {
    id: u64,
    operation: Operation<N>,
}

enum Reply<const N: usize> {
    Read(Vec<u8, N>),
    Written(usize),
    Flushed,
}

struct Response<const N: usize> {
    id: u64,
    result: Result<Reply<N>, ErrorKind>,
}

pub struct Bridge<M: RawMutex, const N: usize> {
    requests: Channel<M, Request<N>, 1>,
    responses: Signal<M, Response<N>>,
}

impl<M: RawMutex, const N: usize> Default for Bridge<M, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<M: RawMutex, const N: usize> Bridge<M, N> {
    pub const fn new() -> Self {
        assert!(N > 0);
        Self {
            requests: Channel::new(),
            responses: Signal::new(),
        }
    }

    /// One client and one socket owner; neither endpoint can be cloned.
    pub fn split(&mut self) -> (Client<'_, M, N>, Server<'_, M, N>) {
        (
            Client {
                bridge: self,
                id: 0,
            },
            Server { bridge: self },
        )
    }
}

pub struct Client<'a, M: RawMutex, const N: usize> {
    bridge: &'a Bridge<M, N>,
    id: u64,
}

pub struct Server<'a, M: RawMutex, const N: usize> {
    bridge: &'a Bridge<M, N>,
}

impl<M: RawMutex, const N: usize> Client<'_, M, N> {
    async fn call(&mut self, operation: Operation<N>) -> Result<Reply<N>, ErrorKind> {
        self.id = self.id.wrapping_add(1);
        let id = self.id;
        self.bridge.requests.send(Request { id, operation }).await;
        loop {
            let response = self.bridge.responses.wait().await;
            // A cancelled operation may already have reached the socket owner.
            // Its late reply must never satisfy the following operation.
            if response.id == id {
                return response.result;
            }
        }
    }
}

impl<M: RawMutex, const N: usize> ErrorType for Client<'_, M, N> {
    type Error = ErrorKind;
}

impl<M: RawMutex, const N: usize> Read for Client<'_, M, N> {
    async fn read(&mut self, buf: &mut [u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        match self.call(Operation::Read(buf.len().min(N))).await? {
            Reply::Read(bytes) if bytes.len() <= buf.len() => {
                buf[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            _ => Err(ErrorKind::InvalidData),
        }
    }
}

impl<M: RawMutex, const N: usize> Write for Client<'_, M, N> {
    async fn write(&mut self, buf: &[u8]) -> Result<usize, Self::Error> {
        if buf.is_empty() {
            return Ok(0);
        }
        let bytes = Vec::from_slice(&buf[..buf.len().min(N)]).unwrap();
        match self.call(Operation::Write(bytes)).await? {
            Reply::Written(len) => Ok(len),
            _ => Err(ErrorKind::InvalidData),
        }
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        match self.call(Operation::Flush).await? {
            Reply::Flushed => Ok(()),
            _ => Err(ErrorKind::InvalidData),
        }
    }
}

impl<M: RawMutex, const N: usize> Server<'_, M, N> {
    /// Serve one half of a duplex socket. A separate bridge serves writes, so
    /// a pending read cannot block a response (HTTP reads and writes overlap).
    pub async fn serve_read<R: Read>(&mut self, socket: &mut R) -> ! {
        loop {
            let request = self.bridge.requests.receive().await;
            let result = match request.operation {
                Operation::Read(len) => {
                    let mut bytes = Vec::new();
                    bytes.resize_default(len).unwrap();
                    socket
                        .read(&mut bytes)
                        .await
                        .map(|len| {
                            bytes.truncate(len);
                            Reply::Read(bytes)
                        })
                        .map_err(|error| error.kind())
                }
                _ => Err(ErrorKind::Unsupported),
            };
            self.bridge.responses.signal(Response {
                id: request.id,
                result,
            });
        }
    }

    /// Serve the independent write half; see `serve_read`.
    pub async fn serve_write<W: Write>(&mut self, socket: &mut W) -> ! {
        loop {
            let request = self.bridge.requests.receive().await;
            let result = match request.operation {
                Operation::Write(bytes) => socket
                    .write(&bytes)
                    .await
                    .map(Reply::Written)
                    .map_err(|error| error.kind()),
                Operation::Flush => socket
                    .flush()
                    .await
                    .map(|()| Reply::Flushed)
                    .map_err(|error| error.kind()),
                _ => Err(ErrorKind::Unsupported),
            };
            self.bridge.responses.signal(Response {
                id: request.id,
                result,
            });
        }
    }

    /// Pump bounded I/O requests on the socket's own executor. Cancellation
    /// drops the current socket operation; no caller-owned buffers are retained.
    pub async fn serve<S: Read + Write>(&mut self, socket: &mut S) -> ! {
        loop {
            let request = self.bridge.requests.receive().await;
            let result = match request.operation {
                Operation::Read(len) => {
                    let mut bytes = Vec::new();
                    bytes.resize_default(len).unwrap();
                    socket.read(&mut bytes).await.map(|len| {
                        bytes.truncate(len);
                        Reply::Read(bytes)
                    })
                }
                Operation::Write(bytes) => socket.write(&bytes).await.map(Reply::Written),
                Operation::Flush => socket.flush().await.map(|()| Reply::Flushed),
            };
            // Only one client operation is live. Replacing an unconsumed reply
            // from a cancelled call avoids blocking the server on stale replies.
            self.bridge.responses.signal(Response {
                id: request.id,
                result: result.map_err(|error| error.kind()),
            });
        }
    }

    /// Discard a closed connection's queued I/O after cancelling `serve`.
    /// The caller must also cancel the client's operation before calling this,
    /// and acknowledge closure before allowing that client to reconnect.
    pub fn reset(&mut self) {
        self.bridge.requests.clear();
        self.bridge.responses.reset();
    }
}
