//! A TLS-terminating proxy in front of the test server, for the Store's TLS
//! settings.
//!
//! The test images run Postgres without TLS. This proxy answers the
//! client's SSLRequest itself, as a Postgres server with TLS on would, does
//! the TLS handshake with a certificate the test chose, and relays the
//! decrypted stream to the server in plain text. A client that sends no
//! SSLRequest is relayed in plain text. [`Answer::Refuse`] answers `N`
//! instead, as a server without TLS does.

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::rustls::{self, ServerConfig};
use tokio_rustls::TlsAcceptor;

const SSL_REQUEST: u32 = 80_877_103;

/// A certificate authority made for one test, which no platform trusts.
pub struct TestCa {
    params: CertificateParams,
    key: KeyPair,
    /// The CA certificate, PEM-encoded, for the Store to trust.
    pub pem: String,
}

impl TestCa {
    pub fn new() -> TestCa {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(DnType::CommonName, "gnomon test CA");
        let pem = params.self_signed(&key).unwrap().pem();
        TestCa { params, key, pem }
    }

    /// A server certificate for `names`, which may be DNS names or IP
    /// addresses, signed by this CA.
    pub fn server_cert(&self, names: &[&str]) -> ServerCert {
        let key = KeyPair::generate().unwrap();
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let params = CertificateParams::new(names).unwrap();
        let issuer = Issuer::from_params(&self.params, &self.key);
        let cert = params.signed_by(&key, &issuer).unwrap();
        ServerCert {
            chain: vec![cert.der().clone()],
            key: key.serialize_der(),
        }
    }
}

pub struct ServerCert {
    chain: Vec<CertificateDer<'static>>,
    key: Vec<u8>,
}

/// What the proxy answers to an SSLRequest.
pub enum Answer {
    /// `S`, then a TLS handshake with this certificate.
    Tls(ServerCert),
    /// `N`: this server does not do TLS.
    Refuse,
}

/// Starts the proxy in front of `upstream` and returns its address.
pub async fn proxy(upstream: SocketAddr, answer: Answer) -> SocketAddr {
    let acceptor = match answer {
        Answer::Tls(cert) => {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let config = ServerConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    cert.chain,
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(cert.key)),
                )
                .unwrap();
            Some(TlsAcceptor::from(Arc::new(config)))
        }
        Answer::Refuse => None,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((client, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = serve(client, upstream, acceptor).await;
            });
        }
    });
    addr
}

async fn serve(
    mut client: TcpStream,
    upstream: SocketAddr,
    acceptor: Option<TlsAcceptor>,
) -> std::io::Result<()> {
    let mut server = TcpStream::connect(upstream).await?;
    let mut first = [0u8; 8];
    client.read_exact(&mut first).await?;
    let len = u32::from_be_bytes(first[0..4].try_into().unwrap());
    let code = u32::from_be_bytes(first[4..8].try_into().unwrap());
    if len != 8 || code != SSL_REQUEST {
        // A plain-text client: its startup message goes straight through.
        server.write_all(&first).await?;
        tokio::io::copy_bidirectional(&mut client, &mut server).await?;
        return Ok(());
    }
    match acceptor {
        Some(acceptor) => {
            client.write_all(b"S").await?;
            let mut tls = acceptor.accept(client).await?;
            tokio::io::copy_bidirectional(&mut tls, &mut server).await?;
        }
        None => {
            client.write_all(b"N").await?;
            tokio::io::copy_bidirectional(&mut client, &mut server).await?;
        }
    }
    Ok(())
}
