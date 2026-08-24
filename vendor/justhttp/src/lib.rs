//! # Simple usage
//!
//! ## Creating the server
//!
//! The easiest way to create a server is to call `Server::http()`.
//!
//! The `http()` function returns an `IoResult<Server>` which will return an error
//! in the case where the server creation fails (for example if the listening port is already
//! occupied).
//!
//! ```no_run
//! let server = justhttp::Server::http("0.0.0.0:0").unwrap();
//! ```
//!
//! A newly-created `Server` will immediately start listening for incoming connections and HTTP
//! requests.
//!
//! ## Receiving requests
//!
//! Calling `server.recv()` will block until the next request is available.
//! This function returns an `IoResult<Request>`, so you need to handle the possible errors.
//!
//! ```no_run
//! # let server = justhttp::Server::http("0.0.0.0:0").unwrap();
//!
//! loop {
//!     // blocks until the next request is received
//!     let request = match server.recv() {
//!         Ok(rq) => rq,
//!         Err(e) => { println!("error: {}", e); break }
//!     };
//!
//!     // do something with the request
//!     // ...
//! }
//! ```
//!
//! In a real-case scenario, you will probably want to spawn multiple worker tasks and call
//! `server.recv()` on all of them. Like this:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::thread;
//! # let server = justhttp::Server::http("0.0.0.0:0").unwrap();
//! let server = Arc::new(server);
//! let mut guards = Vec::with_capacity(4);
//!
//! for _ in (0 .. 4) {
//!     let server = server.clone();
//!
//!     let guard = thread::spawn(move || {
//!         loop {
//!             let rq = server.recv().unwrap();
//!
//!             // ...
//!         }
//!     });
//!
//!     guards.push(guard);
//! }
//! ```
//!
//! If you don't want to block, you can call `server.try_recv()` instead.
//!
//! ## Handling requests
//!
//! The `Request` object returned by `server.recv()` contains informations about the client's request.
//! The most useful methods are probably `request.method()` and `request.url()` which return
//! the requested method (`GET`, `POST`, etc.) and url.
//!
//! To handle a request, you need to create a `Response` object. See the docs of this object for
//! more infos. Here is an example of creating a `Response` from a string, and responding:
//!
//! ```no_run
//! # let server = justhttp::Server::http("0.0.0.0:0").unwrap();
//! # let request = server.recv().unwrap();
//! let response = justhttp::Response::from_string("hello world");
//! let _ = request.respond(response);
//! ```
#![forbid(unsafe_code)]
#![deny(rust_2018_idioms)]

use std::error::Error;
use std::io::Error as IoError;
use std::io::Result as IoResult;
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use conn::ClientConnection;
use stream::Connection;
use pool::MessagesQueue;

pub use http::{HttpVersion, Header, HeaderField, Method, StatusCode};
pub use stream::ListenAddr;
pub use request::Request;
pub use response::Response;

mod conn;
mod http;
mod stream;
mod request;
mod response;
mod pool;

/// The main class of this library.
///
/// Destroying this object will immediately close the listening socket and the reading
///  part of all the client's connections. Requests that have already been returned by
///  the `recv()` function will not close and the responses will be transferred to the client.
pub struct Server {
    // should be false as long as the server exists
    // when set to true, all the subtasks will close within a few hundreds ms
    close: Arc<AtomicBool>,

    // queue for messages received by child threads
    messages: Arc<MessagesQueue<Message>>,

    // result of TcpListener::local_addr()
    listening_addr: ListenAddr,
}

enum Message {
    Error(IoError),
    NewRequest(Request),
}

impl From<IoError> for Message {
    fn from(e: IoError) -> Message {
        Message::Error(e)
    }
}

impl From<Request> for Message {
    fn from(rq: Request) -> Message {
        Message::NewRequest(rq)
    }
}

// compile-time proof that Server can be shared across threads
const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<Server>;
};

pub struct IncomingRequests<'a> {
    server: &'a Server,
}

impl Server {
    /// A server on a TCP address.
    #[inline]
    pub fn http<A>(addr: A) -> Result<Server, Box<dyn Error + Send + Sync + 'static>>
    where
        A: ToSocketAddrs,
    {
        let listener = std::net::TcpListener::bind(addr)?;
        Self::start(stream::Listener::Tcp(listener))
    }

    #[cfg(unix)]
    #[inline]
    /// A server on a UNIX socket at a specific path.
    pub fn http_unix(
        path: &std::path::Path,
    ) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        Self::start(stream::Listener::Unix(listener))
    }

    /// Spawns the accept thread over a bound listener.
    fn start(listener: stream::Listener) -> Result<Server, Box<dyn Error + Send + Sync + 'static>> {
        // building the "close" variable
        let close_trigger = Arc::new(AtomicBool::new(false));

        // building the TcpListener
        let (server, local_addr) = {
            let local_addr = listener.local_addr()?;
            (listener, local_addr)
        };

        // creating a task where server.accept() is continuously called
        // and ClientConnection objects are pushed in the messages queue
        let messages = MessagesQueue::with_capacity(8);

        let inside_close_trigger = close_trigger.clone();
        let inside_messages = messages.clone();
        thread::spawn(move || {
            // a tasks pool is used to dispatch the connections into threads
            let tasks_pool = pool::TaskPool::new();

            // The ceiling on how long a single
            // response write may block before the connection is dropped. A dead
            // reader is finite, not precise — 10s reads as "this peer is gone,"
            // and the edge proxy owns precise per-request timeouts. Because
            // write_all resets this per syscall on any forward progress, the
            // real reclaim is somewhat longer, which is fine for a backstop.
            const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

            while !inside_close_trigger.load(Relaxed) {
                let new_client = match server.accept() {
                    Ok((sock, _)) => {
                        use crate::stream::RefinedTcpStream;
                        // Bound how long a single response write may block, so
                        // a client that stops reading cannot park a server
                        // thread inside `write` forever. Read side is left
                        // untouched — keep-alive connections must wait
                        // indefinitely between requests. Only a fully stalled
                        // peer trips it; a draining client resets the timer
                        // every write. Best-effort — a socket that rejects the
                        // option just keeps tiny_http's original behavior.
                        let _ = sock.set_write_timeout(Some(WRITE_TIMEOUT));
                        let (read_closable, write_closable) = RefinedTcpStream::new(sock);

                        Ok(ClientConnection::new(write_closable, read_closable))
                    }
                    Err(e) => Err(e),
                };

                match new_client {
                    Ok(client) => {
                        let messages = inside_messages.clone();
                        let mut client = Some(client);
                        tasks_pool.spawn(Box::new(move || {
                            if let Some(client) = client.take() {
                                for rq in client {
                                    messages.push(rq.into());
                                }
                            }
                        }));
                    }

                    Err(e) => {
                        // surface the accept error through recv(), then stop accepting
                        inside_messages.push(e.into());
                        break;
                    }
                }
            }
        });

        // result
        Ok(Server {
            messages,
            close: close_trigger,
            listening_addr: local_addr,
        })
    }

    /// Returns an iterator for all the incoming requests.
    ///
    /// The iterator will return `None` if the server socket is shutdown.
    #[inline]
    pub fn incoming_requests(&self) -> IncomingRequests<'_> {
        IncomingRequests { server: self }
    }

    /// Returns the address the server is listening to.
    #[inline]
    pub fn server_addr(&self) -> ListenAddr {
        self.listening_addr.clone()
    }

    /// Blocks until an HTTP request has been submitted and returns it.
    pub fn recv(&self) -> IoResult<Request> {
        match self.messages.pop() {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(rq),
            None => Err(IoError::other("thread unblocked")),
        }
    }

    /// Same as `recv()` but doesn't block longer than timeout
    pub fn recv_timeout(&self, timeout: Duration) -> IoResult<Option<Request>> {
        match self.messages.pop_timeout(timeout) {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Same as `recv()` but doesn't block.
    pub fn try_recv(&self) -> IoResult<Option<Request>> {
        match self.messages.try_pop() {
            Some(Message::Error(err)) => Err(err),
            Some(Message::NewRequest(rq)) => Ok(Some(rq)),
            None => Ok(None),
        }
    }

    /// Unblock thread stuck in recv() or incoming_requests().
    /// If there are several such threads, only one is unblocked.
    /// This method allows graceful shutdown of server.
    pub fn unblock(&self) {
        self.messages.unblock();
    }
}

impl Iterator for IncomingRequests<'_> {
    type Item = Request;
    fn next(&mut self) -> Option<Request> {
        self.server.recv().ok()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.close.store(true, Relaxed);
        // Connect briefly to ourselves to unblock the accept thread
        let maybe_stream = match &self.listening_addr {
            ListenAddr::Ip(addr) => TcpStream::connect(addr).map(Connection::from),
            #[cfg(unix)]
            ListenAddr::Unix(addr) => {
                // TODO: use connect_addr when its stabilized.
                let path = addr.as_pathname().unwrap();
                std::os::unix::net::UnixStream::connect(path).map(Connection::from)
            }
        };
        if let Ok(stream) = maybe_stream {
            let _ = stream.shutdown(Shutdown::Both);
        }

        #[cfg(unix)]
        if let ListenAddr::Unix(addr) = &self.listening_addr {
            if let Some(path) = addr.as_pathname() {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
