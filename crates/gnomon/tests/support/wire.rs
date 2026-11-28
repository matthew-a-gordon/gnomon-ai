//! A pass-through TCP proxy that records what a client sends Postgres.
//!
//! It parses only the frontend side of the wire protocol. It logs each simple
//! Query and each Parse with its statement name. That lets a test check, from
//! outside the Store, which statements reach the server and whether any of
//! them is named, as a transaction-mode pooler would see them. It also keeps
//! each message's bytes, so [`WireLog::replayable_since`] can read the
//! parameters too and give back what the client sent, to send again.
//! Every byte is forwarded unchanged. It keeps each connection's messages
//! apart, and [`WireLog::cut_open_connections`] makes every connection open
//! at that moment close, unseen by the client, when the client next sends
//! on it, as a server or a network that dropped an idle connection would.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// A message the client sent, as the proxy saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sent {
    /// A simple-protocol Query.
    Query(String),
    /// An extended-protocol Parse. An empty name is the unnamed statement.
    Parse { name: String, sql: String },
    /// Any other message, by its type byte.
    Other(u8),
}

/// A statement the client sent, with what it takes to send it again.
#[derive(Debug, Clone)]
pub enum Replayable {
    /// A simple-protocol Query.
    Simple(String),
    /// An extended-protocol statement: its Parse and the Bind after it.
    Extended {
        sql: String,
        /// The parameters' type OIDs.
        types: Vec<u32>,
        /// Each parameter in the binary format, `None` for null.
        params: Vec<Option<Vec<u8>>>,
    },
}

#[derive(Clone, Default)]
pub struct WireLog(Arc<Mutex<Log>>, Arc<tokio::sync::Notify>);

#[derive(Default)]
struct Log {
    /// Every message in the order the proxy saw it, with the number of the
    /// connection it came on.
    sent: Vec<(usize, Sent)>,
    /// Each message's body, in the same order.
    bodies: Vec<Vec<u8>>,
    /// How many connections the proxy has accepted.
    connections: usize,
    /// Connections numbered below this close on their next message.
    cut_below: usize,
}

impl WireLog {
    /// Every message, on every connection, in the order the proxy saw them.
    pub fn sent(&self) -> Vec<Sent> {
        self.0
            .lock()
            .unwrap()
            .sent
            .iter()
            .map(|(_, m)| m.clone())
            .collect()
    }

    /// The statements among the messages after the first `from`, in order.
    /// It suits a client that used one connection meanwhile, and sends its
    /// parameters in the binary format, as `tokio-postgres` does.
    pub fn replayable_since(&self, from: usize) -> Vec<Replayable> {
        let log = self.0.lock().unwrap();
        let mut out = vec![];
        // The SQL and parameter types of the last Parse, until its Bind.
        let mut parsed = None;
        for ((_, message), body) in log.sent.iter().zip(&log.bodies).skip(from) {
            match message {
                Sent::Query(sql) => out.push(Replayable::Simple(sql.clone())),
                Sent::Parse { sql, .. } => {
                    // The name and the SQL, then the parameter types.
                    let mut rest = Body(body);
                    rest.cstring();
                    rest.cstring();
                    let types = (0..rest.u16()).map(|_| rest.u32()).collect();
                    parsed = Some((sql.clone(), types));
                }
                Sent::Other(b'B') => {
                    let (sql, types) = parsed.take().expect("a Parse before its Bind");
                    // The portal and statement names, then the parameters'
                    // formats.
                    let mut rest = Body(body);
                    rest.cstring();
                    rest.cstring();
                    for _ in 0..rest.u16() {
                        assert_eq!(rest.u16(), 1, "a parameter in the text format");
                    }
                    let params = (0..rest.u16())
                        .map(|_| {
                            // A length of -1 is a null.
                            let len = rest.u32() as i32;
                            usize::try_from(len).ok().map(|n| rest.take(n).to_vec())
                        })
                        .collect();
                    out.push(Replayable::Extended { sql, types, params });
                }
                Sent::Other(_) => {}
            }
        }
        out
    }

    /// Each connection's messages, in the order the connections were
    /// accepted.
    pub fn by_connection(&self) -> Vec<Vec<Sent>> {
        let log = self.0.lock().unwrap();
        let mut out = vec![Vec::new(); log.connections];
        for (c, m) in &log.sent {
            out[*c].push(m.clone());
        }
        out
    }

    /// How many messages the proxy has seen, on every connection.
    pub fn len(&self) -> usize {
        self.0.lock().unwrap().sent.len()
    }

    /// Returns once the proxy has seen `n` messages, on every connection.
    pub async fn until_seen(&self, n: usize) {
        loop {
            let seen = self.1.notified();
            if self.len() >= n {
                return;
            }
            seen.await;
        }
    }

    /// How many connections the proxy has accepted.
    pub fn connections(&self) -> usize {
        self.0.lock().unwrap().connections
    }

    /// Makes every connection open now close when the client next sends on
    /// it. That message is logged and never reaches the server. Connections
    /// accepted later are not cut.
    pub fn cut_open_connections(&self) {
        let mut log = self.0.lock().unwrap();
        log.cut_below = log.connections;
    }

    fn accept(&self) -> usize {
        let mut log = self.0.lock().unwrap();
        log.connections += 1;
        log.connections - 1
    }

    /// Logs a message, and says whether its connection is cut.
    fn push(&self, connection: usize, m: Sent, body: &[u8]) -> bool {
        let cut = {
            let mut log = self.0.lock().unwrap();
            log.sent.push((connection, m));
            log.bodies.push(body.to_vec());
            connection < log.cut_below
        };
        self.1.notify_waiters();
        cut
    }
}

/// Starts a proxy in front of `upstream` and returns its address.
pub async fn record(upstream: SocketAddr) -> (SocketAddr, WireLog) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let log = WireLog::default();
    let accept_log = log.clone();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let log = accept_log.clone();
            let connection = log.accept();
            tokio::spawn(async move {
                let server = TcpStream::connect(upstream).await.unwrap();
                let _ = relay(client, server, log, connection).await;
            });
        }
    });
    (addr, log)
}

async fn relay(
    client: TcpStream,
    server: TcpStream,
    log: WireLog,
    connection: usize,
) -> std::io::Result<()> {
    let (mut from_client, mut to_server) = (client, server);
    let (mut client_read, mut client_write) = from_client.split();
    let (mut server_read, mut server_write) = to_server.split();

    let upstream = async {
        startup(&mut client_read, &mut server_write).await?;
        loop {
            let mut header = [0u8; 5];
            client_read.read_exact(&mut header).await?;
            let len = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
            let mut body = vec![0u8; len - 4];
            client_read.read_exact(&mut body).await?;
            if log.push(connection, parse(header[0], &body), &body) {
                // Cut: both sockets close when the relay returns.
                return Ok(());
            }
            // One write per message, as a client makes: Supavisor refuses
            // an authentication message that arrives in two reads.
            let mut message = header.to_vec();
            message.extend_from_slice(&body);
            server_write.write_all(&message).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), std::io::Error>(())
    };
    let downstream = tokio::io::copy(&mut server_read, &mut client_write);
    tokio::select! {
        r = upstream => r,
        r = downstream => r.map(|_| ()),
    }
}

/// Forwards the untyped startup message, and an SSL or GSS request ahead of
/// it if the client sends one.
async fn startup(
    client: &mut (impl AsyncReadExt + Unpin),
    server: &mut (impl AsyncWriteExt + Unpin),
) -> std::io::Result<()> {
    const SSL_REQUEST: u32 = 80_877_103;
    const GSSENC_REQUEST: u32 = 80_877_104;
    loop {
        let mut len = [0u8; 4];
        client.read_exact(&mut len).await?;
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize - 4];
        client.read_exact(&mut body).await?;
        let mut message = len.to_vec();
        message.extend_from_slice(&body);
        server.write_all(&message).await?;
        let code = u32::from_be_bytes(body[0..4].try_into().unwrap());
        if code != SSL_REQUEST && code != GSSENC_REQUEST {
            return Ok(());
        }
    }
}

/// The part of a message's body not yet read.
struct Body<'a>(&'a [u8]);

impl<'a> Body<'a> {
    fn take(&mut self, n: usize) -> &'a [u8] {
        let (taken, rest) = self.0.split_at(n);
        self.0 = rest;
        taken
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take(2).try_into().unwrap())
    }

    fn u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().unwrap())
    }

    /// Skips a string and its terminating zero.
    fn cstring(&mut self) {
        let end = self.0.iter().position(|&c| c == 0).expect("a terminator");
        self.take(end + 1);
    }
}

fn parse(kind: u8, body: &[u8]) -> Sent {
    let cstrings = |b: &[u8]| -> Vec<String> {
        b.split(|&c| c == 0)
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect()
    };
    match kind {
        b'Q' => Sent::Query(cstrings(body).remove(0)),
        b'P' => {
            let mut parts = cstrings(body).into_iter();
            Sent::Parse {
                name: parts.next().unwrap_or_default(),
                sql: parts.next().unwrap_or_default(),
            }
        }
        other => Sent::Other(other),
    }
}
