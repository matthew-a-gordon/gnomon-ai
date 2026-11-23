//! How the Store connects: TLS and the statement mode.
//!
//! TLS with certificate verification is the default. The Store verifies the
//! server's certificate chain against trusted roots and checks that the
//! certificate names the host it connected to. Turning verification off,
//! or TLS off, takes an explicit [`Tls`] setting. The connection string's
//! `sslmode` never turns either off.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::Error;

/// The settings a Store connects with. The default verifies the server's
/// certificate against the platform's trusted roots and sends no named
/// statements.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    pub(crate) tls: Tls,
    pub(crate) named_statements: bool,
}

impl ConnectOptions {
    /// Sets how the Store uses TLS.
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = tls;
        self
    }

    /// Turns named prepared statements on or off. They are off by default,
    /// and every statement is sent unnamed, which works on every
    /// transaction-mode pooler at its default settings (ADR-0001).
    ///
    /// Turn them on only behind a pooler that supports them: PgBouncer 1.21
    /// or later with `max_prepared_statements` above 0, or Supavisor with
    /// `named_prepared_statements` enabled, or with no pooler at all.
    /// Elsewhere calls fail. With them on, the Store prepares each statement
    /// once per connection and reuses it.
    pub fn named_statements(mut self, on: bool) -> Self {
        self.named_statements = on;
        self
    }
}

/// How the Store uses TLS.
#[derive(Clone, Default)]
pub struct Tls(Mode);

#[derive(Clone, Default)]
enum Mode {
    /// TLS, verifying the certificate against the platform's roots.
    #[default]
    PlatformRoots,
    /// TLS, verifying the certificate against these roots only.
    Roots(Arc<RootCertStore>),
    /// TLS without certificate verification.
    Unverified,
    /// No TLS.
    Plaintext,
}

impl std::fmt::Debug for Tls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            Mode::PlatformRoots => "Tls::platform_roots()",
            Mode::Roots(_) => "Tls::trust_pem(..)",
            Mode::Unverified => "Tls::unverified()",
            Mode::Plaintext => "Tls::plaintext()",
        })
    }
}

impl Tls {
    /// TLS, verifying the server's certificate against the platform's
    /// trusted roots. This is the default.
    pub fn platform_roots() -> Tls {
        Tls(Mode::PlatformRoots)
    }

    /// TLS, verifying the server's certificate against the CA certificates
    /// in `pem` only, such as the CA a managed Postgres provider publishes
    /// for its servers.
    pub fn trust_pem(pem: &[u8]) -> Result<Tls, Error> {
        let mut roots = RootCertStore::empty();
        for cert in CertificateDer::pem_slice_iter(pem) {
            let cert = cert.map_err(|e| {
                Error::InvalidConnection(format!("the trusted CA PEM does not parse: {e}"))
            })?;
            roots.add(cert).map_err(|e| {
                Error::InvalidConnection(format!("a trusted CA certificate is invalid: {e}"))
            })?;
        }
        if roots.is_empty() {
            return Err(Error::InvalidConnection(
                "the trusted CA PEM holds no certificate".into(),
            ));
        }
        Ok(Tls(Mode::Roots(Arc::new(roots))))
    }

    /// TLS without verifying the server's certificate. The traffic is
    /// encrypted, but the Store cannot tell the server from anyone who
    /// intercepts the connection. Prefer [`Tls::trust_pem`] with the
    /// server's CA.
    pub fn unverified() -> Tls {
        Tls(Mode::Unverified)
    }

    /// No TLS: the Store connects in plain text, password and data
    /// included. Only for a server reached over a network the deployment
    /// trusts, such as a loopback test server.
    pub fn plaintext() -> Tls {
        Tls(Mode::Plaintext)
    }

    pub(crate) fn is_plaintext(&self) -> bool {
        matches!(self.0, Mode::Plaintext)
    }

    /// The connector for a TLS mode, or `None` for plain text.
    pub(crate) fn connector(&self) -> Result<Option<MakeRustlsConnect>, Error> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::InvalidConnection(format!("TLS: {e}")))?;
        let config = match &self.0 {
            Mode::Plaintext => return Ok(None),
            Mode::PlatformRoots => builder
                .with_root_certificates(platform_roots()?)
                .with_no_client_auth(),
            Mode::Roots(roots) => builder
                .with_root_certificates(roots.clone())
                .with_no_client_auth(),
            Mode::Unverified => builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
                .with_no_client_auth(),
        };
        Ok(Some(MakeRustlsConnect::new(config)))
    }
}

/// The platform's trusted roots, loaded once per process.
fn platform_roots() -> Result<Arc<RootCertStore>, Error> {
    static ROOTS: OnceLock<Arc<RootCertStore>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
        Arc::new(roots)
    });
    if roots.is_empty() {
        return Err(Error::InvalidConnection(
            "the platform has no trusted root certificates; trust the server's CA with \
             Tls::trust_pem"
                .into(),
        ));
    }
    Ok(roots.clone())
}

/// Accepts any certificate, and still checks that the server holds its key.
#[derive(Debug)]
struct AnyCertificate(Arc<CryptoProvider>);

impl ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
