use crate::{Endpoint, HEADER_BYTES};
use core::fmt::Write as _;
use embedded_io_async::{Read, Write};

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    Io,
    Response,
    Status(u16),
    Capacity,
}

/// Caller must authenticate the TLS peer before passing the stream here.
/// A redirect is an error: credentials must never follow another destination.
pub async fn post<S: Read + Write>(
    stream: &mut S,
    endpoint: &Endpoint<'_>,
    authorization: &str,
    body: &[u8],
    scratch: &mut [u8; HEADER_BYTES],
) -> Result<(), Error> {
    if !authorization.starts_with("Basic ")
        || authorization.bytes().any(|b| b == b'\r' || b == b'\n')
    {
        return Err(Error::Capacity);
    }
    let header_len = {
        let mut header = Buffer {
            bytes: scratch,
            used: 0,
        };
        write!(header,
        "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/stream+json\r\nVL-Stream-Fields: device\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        endpoint.path, endpoint.authority, authorization, body.len()
        ).map_err(|_| Error::Capacity)?;
        header.used
    };
    stream
        .write_all(&scratch[..header_len])
        .await
        .map_err(|_| Error::Io)?;
    stream.write_all(body).await.map_err(|_| Error::Io)?;
    stream.flush().await.map_err(|_| Error::Io)?;
    // Bound the full response header, including fragmented/slow responses.
    // The caller also bounds the operation with an overall deadline.
    let response = scratch;
    let mut used = 0;
    loop {
        if used == response.len() {
            return Err(Error::Response);
        }
        let count = stream
            .read(&mut response[used..])
            .await
            .map_err(|_| Error::Io)?;
        if count == 0 {
            return Err(Error::Response);
        }
        used += count;
        if response[..used].windows(4).any(|w| w == b"\r\n\r\n") {
            let end = response[..used]
                .windows(2)
                .position(|w| w == b"\r\n")
                .ok_or(Error::Response)?;
            let line = &response[..end];
            if line.len() < 12
                || !(line.starts_with(b"HTTP/1.1 ") || line.starts_with(b"HTTP/1.0 "))
                || !line[9..12].iter().all(u8::is_ascii_digit)
                || (line.len() > 12 && line[12] != b' ')
            {
                return Err(Error::Response);
            }
            let status = u16::from(line[9] - b'0') * 100
                + u16::from(line[10] - b'0') * 10
                + u16::from(line[11] - b'0');
            return if (200..300).contains(&status) {
                Ok(())
            } else {
                Err(Error::Status(status))
            };
        }
    }
}

struct Buffer<'a> {
    bytes: &'a mut [u8],
    used: usize,
}
impl core::fmt::Write for Buffer<'_> {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self.used.checked_add(text.len()).ok_or(core::fmt::Error)?;
        self.bytes
            .get_mut(self.used..end)
            .ok_or(core::fmt::Error)?
            .copy_from_slice(text.as_bytes());
        self.used = end;
        Ok(())
    }
}
