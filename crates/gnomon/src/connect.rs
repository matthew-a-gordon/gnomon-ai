//! How the Store connects: TLS and the connection pool.
//!
//! TLS with certificate verification is the default. The Store verifies the
//! server's certificate chain against trusted roots and checks that the
//! certificate names the host it connected to. Turning verification off,
//! or TLS off, takes an explicit [`Tls`] setting. The connection string's
//! `sslmode` never turns either off.
//!
//! The pool opens a connection only when a call needs one and none is free,
//! keeps at most [`ConnectOptions::max_connections`], and runs no keepalive
//! or reaper task (ADR-0016). An idle connection is checked when a call
//! takes it, never in the background.

use std::borrow::Cow;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres::config::SslMode;
use tokio_postgres::{Config, NoTls};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::Error;

/// The default of [`ConnectOptions::max_connections`].
pub const DEFAULT_MAX_CONNECTIONS: usize = 10;

/// The default of [`ConnectOptions::wait_timeout`].
pub const DEFAULT_WAIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The settings a Store connects with. The default verifies the server's
/// certificate against the platform's trusted roots, keeps at most
/// [`DEFAULT_MAX_CONNECTIONS`] connections, closes none for being idle,
/// and waits at most [`DEFAULT_WAIT_TIMEOUT`] for a free one.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    pub(crate) tls: Tls,
    pub(crate) max_connections: usize,
    pub(crate) idle_timeout: Option<Duration>,
    pub(crate) wait_timeout: Duration,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        ConnectOptions {
            tls: Tls::default(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            idle_timeout: None,
            wait_timeout: DEFAULT_WAIT_TIMEOUT,
        }
    }
}

impl ConnectOptions {
    /// Sets how the Store uses TLS.
    pub fn tls(mut self, tls: Tls) -> Self {
        self.tls = tls;
        self
    }

    /// Sets the most connections the Store keeps open at once, at least 1.
    /// The default is [`DEFAULT_MAX_CONNECTIONS`]; the bindings set 2. Each
    /// single call and each open [`Transaction`](crate::Transaction) holds
    /// one while it runs.
    pub fn max_connections(mut self, max: usize) -> Self {
        self.max_connections = max;
        self
    }

    /// Closes a connection that has been idle in the pool for longer than
    /// `idle`, when a call would take it next, and opens a new one instead.
    /// Nothing closes it before then, because no background task runs. By
    /// default no connection is closed for being idle.
    pub fn idle_timeout(mut self, idle: Option<Duration>) -> Self {
        self.idle_timeout = idle;
        self
    }

    /// Sets how long a call waits for a free connection when all
    /// [`max_connections`](Self::max_connections) are in use, before it
    /// fails with [`Error::Unavailable`]. The default is
    /// [`DEFAULT_WAIT_TIMEOUT`].
    pub fn wait_timeout(mut self, wait: Duration) -> Self {
        self.wait_timeout = wait;
        self
    }

    /// The connection pool these settings describe, for `connection`. It
    /// opens no connection.
    pub(crate) fn pool(&self, connection: &str) -> Result<deadpool_postgres::Pool, Error> {
        use deadpool_postgres::{
            Manager, ManagerConfig, Pool, QueueMode, RecyclingMethod, Runtime,
        };

        if self.max_connections == 0 {
            return Err(Error::InvalidConnection(
                "the maximum number of connections must be at least 1".into(),
            ));
        }
        let (connection, verify_full) = accept_verify_full(connection);
        let mut config: Config = connection.parse().map_err(|e| {
            Error::InvalidConnection(format!("the connection string does not parse: {e}"))
        })?;
        let ssl_mode = config.get_ssl_mode();
        // Recycling checks only that the client has not seen its connection
        // close, so a healthy call sends no extra query. A connection that
        // died unseen fails when its transaction starts, and the Store then
        // replaces it once (`store.rs`).
        let manager_config = ManagerConfig {
            recycling_method: RecyclingMethod::Fast,
        };
        let manager = match self.tls.connector()? {
            None => {
                if matches!(ssl_mode, SslMode::Require) {
                    return Err(Error::InvalidConnection(format!(
                        "the connection string sets sslmode={}, but the Store's TLS setting is \
                         Tls::plaintext()",
                        if verify_full {
                            "verify-full"
                        } else {
                            "require"
                        }
                    )));
                }
                config.ssl_mode(SslMode::Disable);
                Manager::from_config(config, NoTls, manager_config)
            }
            Some(tls) => {
                if matches!(ssl_mode, SslMode::Disable) {
                    return Err(Error::InvalidConnection(
                        "the connection string sets sslmode=disable, which does not turn TLS \
                         off; connect with Tls::plaintext() to connect without TLS"
                            .into(),
                    ));
                }
                if verify_full && matches!(self.tls.0, Mode::Unverified) {
                    return Err(Error::InvalidConnection(
                        "the connection string sets sslmode=verify-full, but the Store's TLS \
                         setting is Tls::unverified()"
                            .into(),
                    ));
                }
                // Never fall back to plain text, as libpq's default `prefer` does.
                config.ssl_mode(SslMode::Require);
                Manager::from_config(config, tls, manager_config)
            }
        };
        let mut pool = Pool::builder(manager)
            .max_size(self.max_connections)
            .wait_timeout(Some(self.wait_timeout))
            .queue_mode(QueueMode::Lifo)
            .runtime(Runtime::Tokio1);
        if let Some(idle) = self.idle_timeout {
            pool = pool.pre_recycle(deadpool_postgres::Hook::sync_fn(move |_, metrics| {
                if metrics.last_used() > idle {
                    Err(deadpool_postgres::HookError::message(
                        "idle for longer than the idle timeout",
                    ))
                } else {
                    Ok(())
                }
            }));
        }
        pool.build()
            .map_err(|e| Error::InvalidConnection(format!("the connection pool: {e}")))
    }
}

/// The connection string with `sslmode=verify-full` read as
/// `sslmode=require`, and whether it said `verify-full`. tokio-postgres
/// does not parse `verify-full`, and the Store's default TLS setting
/// already verifies the certificate chain and the host name, as
/// `verify-full` asks. A URL names it as a query parameter, and a key=value
/// string as `sslmode=verify-full`, with optional spaces around `=` and
/// optional single quotes around the value.
fn accept_verify_full(connection: &str) -> (Cow<'_, str>, bool) {
    let is_url = connection.starts_with("postgres://") || connection.starts_with("postgresql://");
    let bytes = connection.as_bytes();
    let mut at = 0;
    while let Some(found) = connection[at..].find("sslmode") {
        let start = at + found;
        at = start + "sslmode".len();
        let before = start.checked_sub(1).map(|i| bytes[i]);
        let starts_token = if is_url {
            matches!(before, Some(b'?' | b'&'))
        } else {
            before.is_none_or(|b| b.is_ascii_whitespace())
        };
        if !starts_token {
            continue;
        }
        let rest = &connection[at..];
        let after_key = if is_url { rest } else { rest.trim_start() };
        let Some(value) = after_key.strip_prefix('=') else {
            continue;
        };
        let value = if is_url { value } else { value.trim_start() };
        let (quoted, value) = match value.strip_prefix('\'') {
            Some(v) => (true, v),
            None => (false, value),
        };
        let Some(tail) = value.strip_prefix("verify-full") else {
            continue;
        };
        let tail = if quoted {
            match tail.strip_prefix('\'') {
                Some(t) => t,
                None => continue,
            }
        } else {
            tail
        };
        let ends_token = if is_url {
            tail.is_empty() || tail.starts_with('&') || tail.starts_with('#')
        } else {
            tail.is_empty() || tail.starts_with(|c: char| c.is_ascii_whitespace())
        };
        if !ends_token {
            continue;
        }
        let rewritten = format!("{}sslmode=require{}", &connection[..start], tail);
        return (Cow::Owned(rewritten), true);
    }
    (Cow::Borrowed(connection), false)
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

    /// The connector for a TLS mode, or `None` for plain text.
    pub(crate) fn connector(&self) -> Result<Option<MakeRustlsConnect>, Error> {
        if matches!(self.0, Mode::Plaintext) {
            return Ok(None);
        }
        // The library never installs a process-wide default provider (ADR-0021).
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| Error::InvalidConnection(format!("TLS: {e}")))?;
        let config = match &self.0 {
            Mode::Plaintext => unreachable!("returned above"),
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

#[cfg(test)]
mod tests {
    use super::accept_verify_full;

    #[test]
    fn sslmode_verify_full_reads_as_require_in_a_url_or_a_key_value_string() {
        for (given, read) in [
            (
                "host=db sslmode=verify-full user=u",
                "host=db sslmode=require user=u",
            ),
            ("host=db sslmode = 'verify-full'", "host=db sslmode=require"),
            ("sslmode=verify-full", "sslmode=require"),
            (
                "postgres://u@db/x?sslmode=verify-full",
                "postgres://u@db/x?sslmode=require",
            ),
            (
                "postgresql://u@db/x?application_name=a&sslmode=verify-full&connect_timeout=5",
                "postgresql://u@db/x?application_name=a&sslmode=require&connect_timeout=5",
            ),
        ] {
            let (rewritten, verify_full) = accept_verify_full(given);
            assert_eq!((rewritten.as_ref(), verify_full), (read, true), "{given}");
        }
        for untouched in [
            "host=db sslmode=require",
            "host=db application_name=sslmode=verify-full",
            "host=db sslmode=verify-fuller",
            "postgres://db/x?xsslmode=verify-full",
            "postgres://db/x?sslmode=verify-ca",
        ] {
            let (rewritten, verify_full) = accept_verify_full(untouched);
            assert_eq!((rewritten.as_ref(), verify_full), (untouched, false));
        }
    }
}
