//! A pass-through TCP proxy that records what a client sends Postgres.
//!
//! It parses only the frontend side of the wire protocol, and only enough to
//! log each simple Query and each Parse with its statement name. That lets a
//! test check, from outside the Store, which statements reach the server and
//! whether any of them is named, as a transaction-mode pooler would see them.
//! Every byte is forwarded unchanged.

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

#[derive(Clone, Default)]
pub struct WireLog(Arc<Mutex<Vec<Sent>>>);

impl WireLog {
    pub fn sent(&self) -> Vec<Sent> {
        self.0.lock().unwrap().clone()
    }

    pub fn clear(&self) {
        self.0.lock().unwrap().clear();
    }

    fn push(&self, m: Sent) {
        self.0.lock().unwrap().push(m);
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
            tokio::spawn(async move {
                let server = TcpStream::connect(upstream).await.unwrap();
                let _ = relay(client, server, log).await;
            });
        }
    });
    (addr, log)
}

async fn relay(client: TcpStream, server: TcpStream, log: WireLog) -> std::io::Result<()> {
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
            log.push(parse(header[0], &body));
            server_write.write_all(&header).await?;
            server_write.write_all(&body).await?;
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
        server.write_all(&len).await?;
        server.write_all(&body).await?;
        let code = u32::from_be_bytes(body[0..4].try_into().unwrap());
        if code != SSL_REQUEST && code != GSSENC_REQUEST {
            return Ok(());
        }
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
