//! HTTP's background socket adapter. Only bounded messages cross threads.
use embassy_sync::{blocking_mutex::raw::CriticalSectionRawMutex, channel::Channel};
use han_logs::io_bridge::{CHUNK_BYTES, Client};
use picoserve::{io::ErrorKind, time::Timer};

pub(crate) type Stream = Client<'static, CriticalSectionRawMutex, CHUNK_BYTES>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Close {
    Abort,
    Graceful,
}

pub(crate) struct Control {
    pub close: Channel<CriticalSectionRawMutex, Close, 1>,
    pub closed: Channel<CriticalSectionRawMutex, (), 1>,
}

impl Control {
    pub const fn new() -> Self {
        Self {
            close: Channel::new(),
            closed: Channel::new(),
        }
    }
}

pub(crate) struct Connection<'a> {
    pub rx: &'a mut Stream,
    pub tx: &'a mut Stream,
    pub control: &'a Control,
}

impl Connection<'_> {
    async fn close(self, mode: Close) {
        // No client I/O survives this point. Wait for the owner to cancel both
        // halves, reset their queues, and close before accepting another client.
        self.control.close.send(mode).await;
        self.control.closed.receive().await;
    }
}

impl<Runtime> picoserve::io::Socket<Runtime> for Connection<'_> {
    type Error = ErrorKind;
    type ReadHalf<'a>
        = &'a mut Stream
    where
        Self: 'a;
    type WriteHalf<'a>
        = &'a mut Stream
    where
        Self: 'a;

    fn split(&mut self) -> (Self::ReadHalf<'_>, Self::WriteHalf<'_>) {
        (self.rx, self.tx)
    }

    async fn abort<T: Timer<Runtime>>(
        self,
        _: &picoserve::Timeouts,
        _: &mut T,
    ) -> Result<(), picoserve::Error<Self::Error>> {
        self.close(Close::Abort).await;
        Ok(())
    }

    async fn shutdown<T: Timer<Runtime>>(
        self,
        _: &picoserve::Timeouts,
        _: &mut T,
    ) -> Result<(), picoserve::Error<Self::Error>> {
        self.close(Close::Graceful).await;
        Ok(())
    }
}
