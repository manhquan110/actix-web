use std::net::Shutdown;
use std::{io, mem, time};

use bytes::{Buf, BufMut, BytesMut};
use futures::{Async, Future, Poll};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_timer::Delay;

use super::error::HttpDispatchError;
use super::settings::ServiceConfig;
use super::{h1, h2, HttpHandler, IoStream};
use http::StatusCode;

const HTTP2_PREFACE: [u8; 14] = *b"PRI * HTTP/2.0";

pub(crate) enum HttpProtocol<T: IoStream, H: HttpHandler + 'static> {
    H1(h1::Http1Dispatcher<T, H>),
    H2(h2::Http2<T, H>),
    Unknown(ServiceConfig<H>, T, BytesMut),
    None,
}

// impl<T: IoStream, H: HttpHandler + 'static> HttpProtocol<T, H> {
//     fn shutdown_(&mut self) {
//         match self {
//             HttpProtocol::H1(ref mut h1) => {
//                 let io = h1.io();
//                 let _ = IoStream::set_linger(io, Some(time::Duration::new(0, 0)));
//                 let _ = IoStream::shutdown(io, Shutdown::Both);
//             }
//             HttpProtocol::H2(ref mut h2) => h2.shutdown(),
//             HttpProtocol::Unknown(_, io, _) => {
//                 let _ = IoStream::set_linger(io, Some(time::Duration::new(0, 0)));
//                 let _ = IoStream::shutdown(io, Shutdown::Both);
//             }
//             HttpProtocol::None => (),
//         }
//     }
// }

enum ProtocolKind {
    Http1,
    Http2,
}

#[doc(hidden)]
pub struct HttpChannel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    proto: HttpProtocol<T, H>,
    ka_timeout: Option<Delay>,
}

impl<T, H> HttpChannel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    pub(crate) fn new(settings: ServiceConfig<H>, io: T) -> HttpChannel<T, H> {
        let ka_timeout = settings.client_timer();

        HttpChannel {
            ka_timeout,
            proto: HttpProtocol::Unknown(settings, io, BytesMut::with_capacity(8192)),
        }
    }
}

impl<T, H> Future for HttpChannel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    type Item = ();
    type Error = HttpDispatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        // keep-alive timer
        if self.ka_timeout.is_some() {
            match self.ka_timeout.as_mut().unwrap().poll() {
                Ok(Async::Ready(_)) => {
                    trace!("Slow request timed out, close connection");
                    let proto = mem::replace(&mut self.proto, HttpProtocol::None);
                    if let HttpProtocol::Unknown(settings, io, buf) = proto {
                        self.proto = HttpProtocol::H1(h1::Http1Dispatcher::for_error(
                            settings,
                            io,
                            StatusCode::REQUEST_TIMEOUT,
                            self.ka_timeout.take(),
                            buf,
                        ));
                        return self.poll();
                    }
                    return Ok(Async::Ready(()));
                }
                Ok(Async::NotReady) => (),
                Err(_) => panic!("Something is really wrong"),
            }
        }

        let mut is_eof = false;
        let kind = match self.proto {
            HttpProtocol::H1(ref mut h1) => return h1.poll(),
            HttpProtocol::H2(ref mut h2) => return h2.poll(),
            HttpProtocol::Unknown(_, ref mut io, ref mut buf) => {
                let mut err = None;
                let mut disconnect = false;
                match io.read_available(buf) {
                    Ok(Async::Ready((read_some, stream_closed))) => {
                        is_eof = stream_closed;
                        // Only disconnect if no data was read.
                        if is_eof && !read_some {
                            disconnect = true;
                        }
                    }
                    Err(e) => {
                        err = Some(e.into());
                    }
                    _ => (),
                }
                if disconnect {
                    debug!("Ignored premature client disconnection");
                    return Ok(Async::Ready(()));
                } else if let Some(e) = err {
                    return Err(e);
                }

                if buf.len() >= 14 {
                    if buf[..14] == HTTP2_PREFACE[..] {
                        ProtocolKind::Http2
                    } else {
                        ProtocolKind::Http1
                    }
                } else {
                    return Ok(Async::NotReady);
                }
            }
            HttpProtocol::None => unreachable!(),
        };

        // upgrade to specific http protocol
        let proto = mem::replace(&mut self.proto, HttpProtocol::None);
        if let HttpProtocol::Unknown(settings, io, buf) = proto {
            match kind {
                ProtocolKind::Http1 => {
                    self.proto = HttpProtocol::H1(h1::Http1Dispatcher::new(
                        settings,
                        io,
                        buf,
                        is_eof,
                        self.ka_timeout.take(),
                    ));
                    return self.poll();
                }
                ProtocolKind::Http2 => {
                    self.proto = HttpProtocol::H2(h2::Http2::new(
                        settings,
                        io,
                        buf.freeze(),
                        self.ka_timeout.take(),
                    ));
                    return self.poll();
                }
            }
        }
        unreachable!()
    }
}

#[doc(hidden)]
pub struct H1Channel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    proto: HttpProtocol<T, H>,
}

impl<T, H> H1Channel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    pub(crate) fn new(settings: ServiceConfig<H>, io: T) -> H1Channel<T, H> {
        H1Channel {
            proto: HttpProtocol::H1(h1::Http1Dispatcher::new(
                settings,
                io,
                BytesMut::with_capacity(8192),
                false,
                None,
            )),
        }
    }
}

impl<T, H> Future for H1Channel<T, H>
where
    T: IoStream,
    H: HttpHandler + 'static,
{
    type Item = ();
    type Error = HttpDispatchError;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.proto {
            HttpProtocol::H1(ref mut h1) => h1.poll(),
            _ => unreachable!(),
        }
    }
}

/// Wrapper for `AsyncRead + AsyncWrite` types
pub(crate) struct WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    io: T,
}

impl<T> WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    pub fn new(io: T) -> Self {
        WrapperStream { io }
    }
}

impl<T> IoStream for WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    #[inline]
    fn shutdown(&mut self, _: Shutdown) -> io::Result<()> {
        Ok(())
    }
    #[inline]
    fn set_nodelay(&mut self, _: bool) -> io::Result<()> {
        Ok(())
    }
    #[inline]
    fn set_linger(&mut self, _: Option<time::Duration>) -> io::Result<()> {
        Ok(())
    }
    #[inline]
    fn set_keepalive(&mut self, _: Option<time::Duration>) -> io::Result<()> {
        Ok(())
    }
}

impl<T> io::Read for WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.io.read(buf)
    }
}

impl<T> io::Write for WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }
    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

impl<T> AsyncRead for WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    #[inline]
    fn read_buf<B: BufMut>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.io.read_buf(buf)
    }
}

impl<T> AsyncWrite for WrapperStream<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    #[inline]
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.io.shutdown()
    }
    #[inline]
    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.io.write_buf(buf)
    }
}
