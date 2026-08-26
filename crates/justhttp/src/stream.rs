//! The socket layer: TCP + unix-socket listeners, and the half-close
//! stream that lets one connection be read and written from two threads.

pub use listen::ListenAddr;
pub(crate) use listen::{Connection, Listener};
pub(crate) use refined::RefinedTcpStream;

mod listen {
    //! Abstractions of Tcp and Unix socket types

    use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
    #[cfg(unix)]
    use std::os::unix::net as unix_net;

    /// Unified listener. Either a [`TcpListener`] or [`std::os::unix::net::UnixListener`]
    pub enum Listener {
        Tcp(TcpListener),
        #[cfg(unix)]
        Unix(unix_net::UnixListener),
    }
    impl Listener {
        pub(crate) fn local_addr(&self) -> std::io::Result<ListenAddr> {
            match self {
                Self::Tcp(l) => l.local_addr().map(ListenAddr::from),
                #[cfg(unix)]
                Self::Unix(l) => l.local_addr().map(ListenAddr::from),
            }
        }

        pub(crate) fn accept(&self) -> std::io::Result<(Connection, Option<SocketAddr>)> {
            match self {
                Self::Tcp(l) => l
                    .accept()
                    .map(|(conn, addr)| (Connection::from(conn), Some(addr))),
                #[cfg(unix)]
                Self::Unix(l) => l.accept().map(|(conn, _)| (Connection::from(conn), None)),
            }
        }
    }
    impl From<TcpListener> for Listener {
        fn from(s: TcpListener) -> Self {
            Self::Tcp(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::UnixListener> for Listener {
        fn from(s: unix_net::UnixListener) -> Self {
            Self::Unix(s)
        }
    }

    /// Unified connection. Either a [`TcpStream`] or [`std::os::unix::net::UnixStream`].
    #[derive(Debug)]
    pub(crate) enum Connection {
        Tcp(TcpStream),
        #[cfg(unix)]
        Unix(unix_net::UnixStream),
    }
    impl std::io::Read for Connection {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(s) => s.read(buf),
                #[cfg(unix)]
                Self::Unix(s) => s.read(buf),
            }
        }
    }
    impl std::io::Write for Connection {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            match self {
                Self::Tcp(s) => s.write(buf),
                #[cfg(unix)]
                Self::Unix(s) => s.write(buf),
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.flush(),
                #[cfg(unix)]
                Self::Unix(s) => s.flush(),
            }
        }
    }
    impl Connection {
        /// Gets the peer's address. Some for TCP, None for Unix sockets.
        pub(crate) fn peer_addr(&mut self) -> std::io::Result<Option<SocketAddr>> {
            match self {
                Self::Tcp(s) => s.peer_addr().map(Some),
                #[cfg(unix)]
                Self::Unix(_) => Ok(None),
            }
        }

        pub(crate) fn shutdown(&self, how: Shutdown) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.shutdown(how),
                #[cfg(unix)]
                Self::Unix(s) => s.shutdown(how),
            }
        }

        /// Bound how long a single write to this socket may block. Without a
        /// timeout, a client that stops reading its response leaves the server
        /// thread parked in a `write` forever. A client that keeps draining
        /// resets the timer on every write; only a fully stalled peer trips it,
        /// after which the write errors and the connection is dropped. (One of
        /// the two hardening behaviors this crate carries; see README.md.)
        pub(crate) fn set_write_timeout(
            &self,
            dur: Option<std::time::Duration>,
        ) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.set_write_timeout(dur),
                #[cfg(unix)]
                Self::Unix(s) => s.set_write_timeout(dur),
            }
        }

        /// Disable Nagle's algorithm on TCP connections. The server writes each
        /// response as one buffered flush, so there is no small-packet spray to
        /// coalesce — but a response that does take more than one write (headers
        /// plus a large chunked body, or the chunked terminator after a full
        /// chunk) must not sit in the kernel waiting on the peer's delayed-ACK
        /// timer. Unix sockets have no Nagle; the arm is a no-op so both
        /// transports behave identically.
        pub(crate) fn set_nodelay(&self, nodelay: bool) -> std::io::Result<()> {
            match self {
                Self::Tcp(s) => s.set_nodelay(nodelay),
                #[cfg(unix)]
                Self::Unix(_) => Ok(()),
            }
        }

        pub(crate) fn try_clone(&self) -> std::io::Result<Self> {
            match self {
                Self::Tcp(s) => s.try_clone().map(Self::from),
                #[cfg(unix)]
                Self::Unix(s) => s.try_clone().map(Self::from),
            }
        }
    }
    impl From<TcpStream> for Connection {
        fn from(s: TcpStream) -> Self {
            Self::Tcp(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::UnixStream> for Connection {
        fn from(s: unix_net::UnixStream) -> Self {
            Self::Unix(s)
        }
    }

    /// Unified listen socket address. Either a [`SocketAddr`] or [`std::os::unix::net::SocketAddr`].
    #[derive(Debug, Clone)]
    pub enum ListenAddr {
        Ip(SocketAddr),
        #[cfg(unix)]
        Unix(unix_net::SocketAddr),
    }
    impl ListenAddr {
        pub fn to_ip(self) -> Option<SocketAddr> {
            match self {
                Self::Ip(s) => Some(s),
                #[cfg(unix)]
                Self::Unix(_) => None,
            }
        }

        /// Gets the Unix socket address.
        ///
        /// This is also available on non-Unix platforms, for ease of use, but always returns `None`.
        #[cfg(unix)]
        pub fn to_unix(self) -> Option<unix_net::SocketAddr> {
            match self {
                Self::Ip(_) => None,
                Self::Unix(s) => Some(s),
            }
        }
    }
    impl From<SocketAddr> for ListenAddr {
        fn from(s: SocketAddr) -> Self {
            Self::Ip(s)
        }
    }
    #[cfg(unix)]
    impl From<unix_net::SocketAddr> for ListenAddr {
        fn from(s: unix_net::SocketAddr) -> Self {
            Self::Unix(s)
        }
    }
    impl std::fmt::Display for ListenAddr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Ip(s) => s.fmt(f),
                #[cfg(unix)]
                Self::Unix(s) => std::fmt::Debug::fmt(s, f),
            }
        }
    }
}

mod refined {
    use std::io::Result as IoResult;
    use std::io::{Read, Write};
    use std::net::{Shutdown, SocketAddr};

    use super::listen::Connection;

    // harbor: upstream wrapped the connection in a Stream enum whose other
    // variant was Https(SslStream); with TLS stripped the enum collapsed away
    // and RefinedTcpStream holds the Connection directly.

    pub struct RefinedTcpStream {
        stream: Connection,
        close_read: bool,
        close_write: bool,
    }

    impl RefinedTcpStream {
        pub(crate) fn new<S>(stream: S) -> (RefinedTcpStream, RefinedTcpStream)
        where
            S: Into<Connection>,
        {
            let stream: Connection = stream.into();

            // same panic surface as upstream: a socket whose fd cannot be
            // duplicated is unusable anyway
            let (read, write) = (stream.try_clone().unwrap(), stream);

            let read = RefinedTcpStream {
                stream: read,
                close_read: true,
                close_write: false,
            };

            let write = RefinedTcpStream {
                stream: write,
                close_read: false,
                close_write: true,
            };

            (read, write)
        }

        pub(crate) fn peer_addr(&mut self) -> IoResult<Option<SocketAddr>> {
            self.stream.peer_addr()
        }
    }

    impl Drop for RefinedTcpStream {
        fn drop(&mut self) {
            if self.close_read {
                self.stream.shutdown(Shutdown::Read).ok();
            }

            if self.close_write {
                self.stream.shutdown(Shutdown::Write).ok();
            }
        }
    }

    impl Read for RefinedTcpStream {
        fn read(&mut self, buf: &mut [u8]) -> IoResult<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for RefinedTcpStream {
        fn write(&mut self, buf: &[u8]) -> IoResult<usize> {
            self.stream.write(buf)
        }

        fn flush(&mut self) -> IoResult<()> {
            self.stream.flush()
        }
    }
}
