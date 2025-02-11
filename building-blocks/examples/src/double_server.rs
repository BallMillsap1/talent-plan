extern crate tokio;
#[macro_use]
extern crate futures;
extern crate bytes;

use bytes::{BufMut, Bytes, BytesMut};
use futures::future::{self, Either};
use futures::sync::mpsc;
use std::env;
use std::io;
use tokio::net::{TcpListener, TcpStream};
use tokio::prelude::*;
use tokio::runtime::Runtime;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

/// Shorthand for the transmit half of the message channel.
type Tx = mpsc::UnboundedSender<Bytes>;

/// Shorthand for the receive half of the message channel.
type Rx = mpsc::UnboundedReceiver<Bytes>;

/// Data that is shared between all peers in the chat server.
///
/// This is the set of `Tx` handles for all connected clients. Whenever a
/// message is received from a client, it is broadcasted to all peers by
/// iterating over the `peers` entries and sending a copy of the message on each
/// `Tx`.
struct Shared {
    peers: HashMap<SocketAddr, Tx>,
}

/// The state for each connected client.
struct CPeer {
    /// Name of the peer.
    ///
    /// When a client connects, the first line sent is treated as the client's
    /// name (like alice or bob). The name is used to preface all messages that
    /// arrive from the client so that we can simulate a real chat server:
    ///
    /// ```text
    /// alice: Hello everyone.
    /// bob: Welcome to telnet chat!
    /// ```
    name: BytesMut,

    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    lines: Lines,

    /// Handle to the shared chat state.
    ///
    /// This is used to broadcast messages read off the socket to all connected
    /// peers.
    c_state: Arc<Mutex<Shared>>,

    /// Handle to the shared chat state.
    ///
    /// This is used to broadcast messages read off the socket to all connected
    /// peers.
    go_state: Arc<Mutex<Shared>>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: Rx,

    /// Client socket address.
    ///
    /// The socket address is used as the key in the `peers` HashMap. The
    /// address is saved so that the `
    /// Peer` drop implementation can clean up its
    /// entry.
    addr: SocketAddr,
}

/// The state for each connected client.
struct GOPeer {
    /// Name of the peer.
    ///
    /// When a client connects, the first line sent is treated as the client's
    /// name (like alice or bob). The name is used to preface all messages that
    /// arrive from the client so that we can simulate a real chat server:
    ///
    /// ```text
    /// alice: Hello everyone.
    /// bob: Welcome to telnet chat!
    /// ```
    name: BytesMut,

    /// The TCP socket wrapped with the `Lines` codec, defined below.
    ///
    /// This handles sending and receiving data on the socket. When using
    /// `Lines`, we can work at the line level instead of having to manage the
    /// raw byte operations.
    lines: Lines,

    /// Handle to the shared chat state.
    ///
    /// This is used to broadcast messages read off the socket to all connected
    /// peers.
    c_state: Arc<Mutex<Shared>>,

    /// Handle to the shared chat state.
    ///
    /// This is used to broadcast messages read off the socket to all connected
    /// peers.
    go_state: Arc<Mutex<Shared>>,

    /// Receive half of the message channel.
    ///
    /// This is used to receive messages from peers. When a message is received
    /// off of this `Rx`, it will be written to the socket.
    rx: Rx,

    /// Client socket address.
    ///
    /// The socket address is used as the key in the `peers` HashMap. The
    /// address is saved so that the `Peer` drop implementation can clean up its
    /// entry.
    addr: SocketAddr,
}

/// Line based codec
///
/// This decorates a socket and presents a line based read / write interface.
///
/// As a user of `Lines`, we can focus on working at the line level. So, we send
/// and receive values that represent entire lines. The `Lines` codec will
/// handle the encoding and decoding as well as reading from and writing to the
/// socket.
#[derive(Debug)]
struct Lines {
    /// The TCP socket.
    socket: TcpStream,

    /// Buffer used when reading from the socket. Data is not returned from this
    /// buffer until an entire line has been read.
    rd: BytesMut,

    /// Buffer used to stage data before writing it to the socket.
    wr: BytesMut,
}

impl Shared {
    /// Create a new, empty, instance of `Shared`.
    fn new() -> Self {
        Shared {
            peers: HashMap::new(),
        }
    }
}

impl CPeer {
    /// Create a new instance of `CPeer`.
    fn new(
        name: BytesMut,
        c_state: Arc<Mutex<Shared>>,
        go_state: Arc<Mutex<Shared>>,
        lines: Lines,
    ) -> CPeer {
        // Get the client socket address
        let addr = lines.socket.peer_addr().unwrap();

        // Create a channel for this peer
        let (tx, rx) = mpsc::unbounded();

        // Add an entry for this `CPeer` in the shared state map.
        c_state.lock().unwrap().peers.insert(addr, tx);

        CPeer {
            name,
            lines,
            c_state,
            go_state,
            rx,
            addr,
        }
    }
}

/// This is where a connected client is managed.
///
/// A `CPeer` is also a future representing completely processing the client.
///
/// When a `CPeer` is created, the first line (representing the client's name)
/// has already been read. When the socket closes, the `CPeer` future completes.
///
/// While processing, the peer future implementation will:
///
/// 1) Receive messages on its message channel and write them to the socket.
/// 2) Receive messages from the socket and broadcast them to all peers.
///
impl Future for CPeer {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        // Tokio (and futures) use cooperative scheduling without any
        // preemption. If a task never yields execution back to the executor,
        // then other tasks may be starved.
        //
        // To deal with this, robust applications should not have any unbounded
        // loops. In this example, we will read at most `LINES_PER_TICK` lines
        // from the client on each tick.
        //
        // If the limit is hit, the current task is notified, informing the
        // executor to schedule the task again asap.
        const LINES_PER_TICK: usize = 10;

        // Receive all messages from peers.
        for i in 0..LINES_PER_TICK {
            // Polling an `UnboundedReceiver` cannot fail, so `unwrap` here is
            // safe.
            match self.rx.poll().unwrap() {
                Async::Ready(Some(v)) => {
                    // Buffer the line. Once all lines are buffered, they will
                    // be flushed to the socket (right below).
                    self.lines.buffer(&v);

                    // If this is the last iteration, the loop will break even
                    // though there could still be lines to read. Because we did
                    // not reach `Async::NotReady`, we have to notify ourselves
                    // in order to tell the executor to schedule the task again.
                    if i + 1 == LINES_PER_TICK {
                        task::current().notify();
                    }
                }
                _ => break,
            }
        }

        // Flush the write buffer to the socket
        let _ = self.lines.poll_flush()?;

        // Read new lines from the socket
        while let Async::Ready(line) = self.lines.poll()? {
            println!("Received line ({:?}) : {:?}", self.name, line);

            if let Some(message) = line {
                // Append the peer's name to the front of the line:
                let mut line = self.name.clone();
                line.extend_from_slice(b": ");
                line.extend_from_slice(&message);
                line.extend_from_slice(b"\r\n");

                // We're using `Bytes`, which allows zero-copy clones (by
                // storing the data in an Arc internally).
                //
                // However, before cloning, we must freeze the data. This
                // converts it from mutable -> immutable, allowing zero copy
                // cloning.
                let line = line.freeze();

                // Now, send the line to all other peers
                for (addr, tx) in &self.go_state.lock().unwrap().peers {
                    // Don't send the message to ourselves
                    if *addr != self.addr {
                        // The send only fails if the rx half has been dropped,
                        // however this is impossible as the `tx` half will be
                        // removed from the map before the `rx` is dropped.
                        tx.unbounded_send(line.clone()).unwrap();
                    }
                }
            } else {
                // EOF was reached. The remote client has disconnected. There is
                // nothing more to do.
                return Ok(Async::Ready(()));
            }
        }

        // As always, it is important to not just return `NotReady` without
        // ensuring an inner future also returned `NotReady`.
        //
        // We know we got a `NotReady` from either `self.rx` or `self.lines`, so
        // the contract is respected.
        Ok(Async::NotReady)
    }
}

impl Drop for CPeer {
    fn drop(&mut self) {
        self.c_state.lock().unwrap().peers.remove(&self.addr);
    }
}

impl GOPeer {
    /// Create a new instance of `CPeer`.
    fn new(
        name: BytesMut,
        c_state: Arc<Mutex<Shared>>,
        go_state: Arc<Mutex<Shared>>,
        lines: Lines,
    ) -> GOPeer {
        // Get the client socket address
        let addr = lines.socket.peer_addr().unwrap();

        // Create a channel for this peer
        let (tx, rx) = mpsc::unbounded();

        // Add an entry for this `CPeer` in the shared state map.
        go_state.lock().unwrap().peers.insert(addr, tx);

        GOPeer {
            name,
            lines,
            c_state,
            go_state,
            rx,
            addr,
        }
    }
}

/// This is where a connected client is managed.
///
/// A `CPeer` is also a future representing completely processing the client.
///
/// When a `CPeer` is created, the first line (representing the client's name)
/// has already been read. When the socket closes, the `CPeer` future completes.
///
/// While processing, the peer future implementation will:
///
/// 1) Receive messages on its message channel and write them to the socket.
/// 2) Receive messages from the socket and broadcast them to all peers.
///
impl Future for GOPeer {
    type Item = ();
    type Error = io::Error;

    fn poll(&mut self) -> Poll<(), io::Error> {
        // Tokio (and futures) use cooperative scheduling without any
        // preemption. If a task never yields execution back to the executor,
        // then other tasks may be starved.
        //
        // To deal with this, robust applications should not have any unbounded
        // loops. In this example, we will read at most `LINES_PER_TICK` lines
        // from the client on each tick.
        //
        // If the limit is hit, the current task is notified, informing the
        // executor to schedule the task again asap.
        const LINES_PER_TICK: usize = 10;

        // Receive all messages from peers.
        for i in 0..LINES_PER_TICK {
            // Polling an `UnboundedReceiver` cannot fail, so `unwrap` here is
            // safe.
            match self.rx.poll().unwrap() {
                Async::Ready(Some(v)) => {
                    // Buffer the line. Once all lines are buffered, they will
                    // be flushed to the socket (right below).
                    self.lines.buffer(&v);

                    // If this is the last iteration, the loop will break even
                    // though there could still be lines to read. Because we did
                    // not reach `Async::NotReady`, we have to notify ourselves
                    // in order to tell the executor to schedule the task again.
                    if i + 1 == LINES_PER_TICK {
                        task::current().notify();
                    }
                }
                _ => break,
            }
        }

        // Flush the write buffer to the socket
        let _ = self.lines.poll_flush()?;

        // Read new lines from the socket
        while let Async::Ready(line) = self.lines.poll()? {
            println!("Received line ({:?}) : {:?}", self.name, line);

            if let Some(message) = line {
                // Append the peer's name to the front of the line:
                let mut line = self.name.clone();
                line.extend_from_slice(b": ");
                line.extend_from_slice(&message);
                line.extend_from_slice(b"\r\n");

                // We're using `Bytes`, which allows zero-copy clones (by
                // storing the data in an Arc internally).
                //
                // However, before cloning, we must freeze the data. This
                // converts it from mutable -> immutable, allowing zero copy
                // cloning.
                let line = line.freeze();

                // Now, send the line to all other peers
                for (addr, tx) in &self.c_state.lock().unwrap().peers {
                    // Don't send the message to ourselves
                    if *addr != self.addr {
                        // The send only fails if the rx half has been dropped,
                        // however this is impossible as the `tx` half will be
                        // removed from the map before the `rx` is dropped.
                        tx.unbounded_send(line.clone()).unwrap();
                    }
                }
            } else {
                // EOF was reached. The remote client has disconnected. There is
                // nothing more to do.
                return Ok(Async::Ready(()));
            }
        }

        // As always, it is important to not just return `NotReady` without
        // ensuring an inner future also returned `NotReady`.
        //
        // We know we got a `NotReady` from either `self.rx` or `self.lines`, so
        // the contract is respected.
        Ok(Async::NotReady)
    }
}

impl Drop for GOPeer {
    fn drop(&mut self) {
        self.go_state.lock().unwrap().peers.remove(&self.addr);
    }
}

impl Lines {
    /// Create a new `Lines` codec backed by the socket
    fn new(socket: TcpStream) -> Self {
        Lines {
            socket,
            rd: BytesMut::new(),
            wr: BytesMut::new(),
        }
    }

    /// Buffer a line.
    ///
    /// This writes the line to an internal buffer. Calls to `poll_flush` will
    /// attempt to flush this buffer to the socket.
    fn buffer(&mut self, line: &[u8]) {
        // Ensure the buffer has capacity. Ideally this would not be unbounded,
        // but to keep the example simple, we will not limit this.
        self.wr.reserve(line.len());

        // Push the line onto the end of the write buffer.
        //
        // The `put` function is from the `BufMut` trait.
        self.wr.put(line);
    }

    /// Flush the write buffer to the socket
    fn poll_flush(&mut self) -> Poll<(), io::Error> {
        // As long as there is buffered data to write, try to write it.
        while !self.wr.is_empty() {
            // Try to write some bytes to the socket
            let n = try_ready!(self.socket.poll_write(&self.wr));

            // As long as the wr is not empty, a successful write should
            // never write 0 bytes.
            assert!(n > 0);

            // This discards the first `n` bytes of the buffer.
            let _ = self.wr.split_to(n);
        }

        Ok(Async::Ready(()))
    }

    /// Read data from the socket.
    ///
    /// This only returns `Ready` when the socket has closed.
    fn fill_read_buf(&mut self) -> Poll<(), io::Error> {
        loop {
            // Ensure the read buffer has capacity.
            //
            // This might result in an internal allocation.
            self.rd.reserve(1024);

            // Read data into the buffer.
            let n = try_ready!(self.socket.read_buf(&mut self.rd));

            if n == 0 {
                return Ok(Async::Ready(()));
            }
        }
    }
}

impl Stream for Lines {
    type Item = BytesMut;
    type Error = io::Error;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        // First, read any new data that might have been received off the socket
        let sock_closed = self.fill_read_buf()?.is_ready();

        // Now, try finding lines
        let pos = self
            .rd
            .windows(2)
            .enumerate()
            .find(|&(_, bytes)| bytes == b"\r\n")
            .map(|(i, _)| i);

        if let Some(pos) = pos {
            // Remove the line from the read buffer and set it to `line`.
            let mut line = self.rd.split_to(pos + 2);

            // Drop the trailing \r\n
            line.split_off(pos);

            // Return the line
            return Ok(Async::Ready(Some(line)));
        }

        if sock_closed {
            Ok(Async::Ready(None))
        } else {
            Ok(Async::NotReady)
        }
    }
}

/// Spawn a task to manage the socket.
///
/// This will read the first line from the socket to identify the client, then
/// add the client to the set of connected peers in the chat service.
fn c_process(socket: TcpStream, c_state: Arc<Mutex<Shared>>, go_state: Arc<Mutex<Shared>>) {
    // Wrap the socket with the `Lines` codec that we wrote above.
    //
    // By doing this, we can operate at the line level instead of doing raw byte
    // manipulation.
    let lines = Lines::new(socket);

    // The first line is treated as the client's name. The client is not added
    // to the set of connected peers until this line is received.
    //
    // We use the `into_future` combinator to extract the first item from the
    // lines stream. `into_future` takes a `Stream` and converts it to a future
    // of `(first, rest)` where `rest` is the original stream instance.
    let connection = lines
        .into_future()
        // `into_future` doesn't have the right error type, so map the error to
        // make it work.
        .map_err(|(e, _)| e)
        // Process the first received line as the client's name.
        .and_then(|(name, lines)| {
            let name = match name {
                Some(name) => name,
                None => {
                    // The remote client closed the connection without sending
                    // any data.
                    return Either::A(future::ok(()));
                }
            };

            println!("`{:?}` is joining the chat", name);

            // Create the peer.
            //
            // This is also a future that processes the connection, only
            // completing when the socket closes.
            let peer = CPeer::new(name, c_state, go_state, lines);

            // Wrap `peer` with `Either::B` to make the return type fit.
            Either::B(peer)
        })
        // Task futures have an error of type `()`, this ensures we handle the
        // error. We do this by printing the error to STDOUT.
        .map_err(|e| {
            println!("connection error = {:?}", e);
        });

    // Spawn the task. Internally, this submits the task to a thread pool.
    tokio::spawn(connection);
}

/// Spawn a task to manage the socket.
///
/// This will read the first line from the socket to identify the client, then
/// add the client to the set of connected peers in the chat service.
fn go_process(socket: TcpStream, c_state: Arc<Mutex<Shared>>, go_state: Arc<Mutex<Shared>>) {
    // Wrap the socket with the `Lines` codec that we wrote above.
    //
    // By doing this, we can operate at the line level instead of doing raw byte
    // manipulation.
    let lines = Lines::new(socket);

    // The first line is treated as the client's name. The client is not added
    // to the set of connected peers until this line is received.
    //
    // We use the `into_future` combinator to extract the first item from the
    // lines stream. `into_future` takes a `Stream` and converts it to a future
    // of `(first, rest)` where `rest` is the original stream instance.
    let connection = lines
        .into_future()
        // `into_future` doesn't have the right error type, so map the error to
        // make it work.
        .map_err(|(e, _)| e)
        // Process the first received line as the client's name.
        .and_then(|(name, lines)| {
            let name = match name {
                Some(name) => name,
                None => {
                    // The remote client closed the connection without sending
                    // any data.
                    return Either::A(future::ok(()));
                }
            };

            println!("`{:?}` is joining the chat", name);

            // Create the peer.
            //
            // This is also a future that processes the connection, only
            // completing when the socket closes.
            let peer = GOPeer::new(name, c_state, go_state, lines);

            // Wrap `peer` with `Either::B` to make the return type fit.
            Either::B(peer)
        })
        // Task futures have an error of type `()`, this ensures we handle the
        // error. We do this by printing the error to STDOUT.
        .map_err(|e| {
            println!("connection error = {:?}", e);
        });

    // Spawn the task. Internally, this submits the task to a thread pool.
    tokio::spawn(connection);
}

pub fn main() -> Result<(), Box<std::error::Error>> {
    let c_addr = env::args().nth(1).unwrap_or("127.0.0.1:8081".to_string());
    let c_listen_addr = c_addr.parse::<SocketAddr>()?;

    let go_addr = env::args().nth(2).unwrap_or("127.0.0.1:8080".to_string());
    let go_listen_addr = go_addr.parse::<SocketAddr>()?;

    println!("Listening on: {}", c_listen_addr);
    let c_socket = TcpListener::bind(&c_listen_addr)?;

    println!("Listening on: {}", go_listen_addr);
    let go_socket = TcpListener::bind(&go_listen_addr)?;

    let c_socket_state = Arc::new(Mutex::new(Shared::new()));
    let go_socket_state = Arc::new(Mutex::new(Shared::new()));
    let c_c_socket_state = c_socket_state.clone();
    let c_go_socket_state = go_socket_state.clone();

    let c_server = c_socket
        .incoming()
        .for_each(move |socket| {
            // Spawn a task to process the connection
            c_process(socket, c_c_socket_state.clone(), c_go_socket_state.clone());
            Ok(())
        })
        .map_err(|err| {
            println!("accept error = {:?}", err);
        });

    let go_server = go_socket
        .incoming()
        .for_each(move |socket| {
            // Spawn a task to process the connection
            go_process(socket, c_socket_state.clone(), go_socket_state.clone());
            Ok(())
        })
        .map_err(|err| {
            println!("accept error = {:?}", err);
        });

    println!("c server running on localhost:8081");
    println!("go server running on localhost:8080");

    // Create the runtime
    let mut rt = Runtime::new().unwrap();
    // Spawn the server task
    rt.spawn(c_server);

    tokio::run(go_server);
    Ok(())
}
