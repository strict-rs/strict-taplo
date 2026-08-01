//! Execution-model-specific JSON Schema transport capabilities.

#[cfg(all(
  feature = "reqwest",
  not(target_arch = "wasm32"),
  any(feature = "native-tls", feature = "rustls-tls")
))]
use std::fs;
use std::future::Future;
#[cfg(all(
  feature = "reqwest",
  not(target_arch = "wasm32"),
  any(feature = "native-tls", feature = "rustls-tls")
))]
use std::io::Error as IoError;
use std::path::Path;
use std::path::PathBuf;
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
use std::time::Duration;

use futures::FutureExt as _;
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
use futures::future::BoxFuture;
use futures::future::LocalBoxFuture;
#[cfg(all(feature = "reqwest", feature = "rustls-tls", not(target_arch = "wasm32")))]
use rustls::crypto::CryptoProvider;
#[cfg(all(feature = "reqwest", feature = "rustls-tls", not(target_arch = "wasm32")))]
use rustls::crypto::ring::default_provider;
use serde_json::Value;
use thiserror::Error;
use time::OffsetDateTime;
use url::Url;

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
use crate::environment::ConcurrentEnvironment;
use crate::environment::Environment;
use crate::environment::EnvironmentError;
use crate::environment::LocalEnvironment;

/// A typed failure while loading or storing schema transport data.
#[derive(Debug, Error)]
pub enum TransportError {
  /// A host environment operation failed.
  #[error(transparent)]
  Environment(#[from] EnvironmentError),
  /// A URL using the `file` scheme could not be mapped into the host path model.
  #[error("schema URL `{url}` is not a valid host file path")]
  InvalidFileUrl {
    /// Rejected file URL.
    url: Url,
  },
  /// The selected transport intentionally has no HTTP capability.
  #[error("remote schema transport is unavailable for `{url}`")]
  RemoteUnavailable {
    /// Remote URL that required a transport.
    url: Url,
  },
  /// The URL scheme is not a supported schema transport.
  #[error("schema transport does not support the `{scheme}` scheme in `{url}`")]
  UnsupportedScheme {
    /// Unsupported URL scheme.
    scheme: String,
    /// Rejected URL.
    url:    Url,
  },
  /// An HTTP request or response operation failed.
  #[cfg(feature = "reqwest")]
  #[error("HTTP schema request failed for `{url}`")]
  Http {
    /// Requested URL.
    url:    Url,
    /// Underlying HTTP failure.
    #[source]
    source: Box<reqwest::Error>,
  },
  /// The HTTP client could not be constructed.
  #[cfg(feature = "reqwest")]
  #[error("failed to initialize the schema HTTP transport")]
  HttpClient {
    /// Underlying client construction failure.
    #[source]
    source: Box<reqwest::Error>,
  },
  /// A configured custom certificate could not be read.
  #[cfg(all(
    feature = "reqwest",
    not(target_arch = "wasm32"),
    any(feature = "native-tls", feature = "rustls-tls")
  ))]
  #[error("failed to read custom certificate `{path}`")]
  CertificateRead {
    /// Certificate path.
    path:   PathBuf,
    /// Underlying filesystem failure.
    #[source]
    source: IoError,
  },
  /// A configured custom certificate could not be decoded.
  #[cfg(all(
    feature = "reqwest",
    not(target_arch = "wasm32"),
    any(feature = "native-tls", feature = "rustls-tls")
  ))]
  #[error("failed to decode custom certificate `{path}`")]
  CertificateDecode {
    /// Certificate path.
    path:   PathBuf,
    /// Underlying certificate decoder failure.
    #[source]
    source: Box<reqwest::Error>,
  },
  /// A custom certificate was configured for a build without TLS support.
  #[cfg(all(
    feature = "reqwest",
    not(target_arch = "wasm32"),
    not(any(feature = "native-tls", feature = "rustls-tls"))
  ))]
  #[error("custom certificate `{path}` requires a TLS-enabled build")]
  TlsUnavailable {
    /// Configured certificate path.
    path: PathBuf,
  },
  /// The selected Rustls crypto provider could not be installed.
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32"), feature = "rustls-tls"))]
  #[error("failed to install the Rustls crypto provider")]
  CryptoProvider,
  /// Bytes loaded through a transport were not valid JSON.
  #[error("schema transport returned invalid JSON for `{url}`")]
  Json {
    /// Source URL.
    url:    Url,
    /// Underlying JSON decoder failure.
    #[source]
    source: serde_json::Error,
  },
}

/// Capability-neutral location selected from one schema URL.
enum SchemaLocation {
  /// Read one host file.
  File(PathBuf),
  /// Read one HTTP or HTTPS resource through the selected transport.
  Remote,
}

/// Read the current clock through the shared typed transport boundary.
fn current_time(environment: &impl Environment) -> Result<OffsetDateTime, TransportError> {
  environment.now().map_err(TransportError::from)
}

/// Classify one path through the shared typed transport boundary.
fn path_is_absolute(environment: &impl Environment, path: &Path) -> Result<bool, TransportError> {
  environment.is_absolute(path).map_err(TransportError::from)
}

/// Decode transport bytes as JSON while preserving the owning URL.
fn decode_json(url: Url, bytes: &[u8]) -> Result<Value, TransportError> {
  serde_json::from_slice(bytes).map_err(|source| TransportError::Json {
    url,
    source,
  })
}

/// Define one execution-model-specific HTTP schema reader.
#[cfg(feature = "reqwest")]
macro_rules! define_http_reader {
  ($name:ident, $future:ident, $boxed:ident, $description:literal) => {
    #[doc = $description]
    fn $name(client: &reqwest::Client, url: Url) -> $future<'_, Result<Vec<u8>, TransportError>> {
      async move {
        let response = client
          .get(url.clone())
          .send()
          .await
          .and_then(reqwest::Response::error_for_status)
          .map_err(|source| TransportError::Http {
            url:    url.clone(),
            source: Box::new(source),
          })?;
        response
          .bytes()
          .await
          .map(|bytes| bytes.to_vec())
          .map_err(|source| TransportError::Http {
            url,
            source: Box::new(source),
          })
      }
      .$boxed()
    }
  };
}

#[cfg(feature = "reqwest")]
define_http_reader!(
  read_http_local,
  LocalBoxFuture,
  boxed_local,
  "Fetch one remote schema through a local future."
);

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
define_http_reader!(
  read_http_concurrent,
  BoxFuture,
  boxed,
  "Fetch one remote schema through a concurrent future."
);

/// Classify one schema URL without choosing a transport capability or asynchronous execution model.
fn classify_schema_location(environment: &impl Environment, url: &Url) -> Result<SchemaLocation, TransportError> {
  match url.scheme() {
    "file" => environment
      .to_file_path_normalized(url)?
      .map(SchemaLocation::File)
      .ok_or_else(|| TransportError::InvalidFileUrl {
        url: url.clone()
      }),
    "http" | "https" => Ok(SchemaLocation::Remote),
    scheme => Err(TransportError::UnsupportedScheme {
      scheme: scheme.to_owned(),
      url:    url.clone(),
    }),
  }
}

/// Read and decode one schema URL through injected file and remote capabilities.
async fn read_json_with<E, ReadFile, ReadFileFuture, ReadRemote, ReadRemoteFuture>(
  environment: &E,
  url: Url,
  read_file: ReadFile,
  read_remote: ReadRemote,
) -> Result<Value, TransportError>
where
  E: Environment,
  ReadFile: FnOnce(PathBuf) -> ReadFileFuture,
  ReadFileFuture: Future<Output = Result<Vec<u8>, EnvironmentError>>,
  ReadRemote: FnOnce(Url) -> ReadRemoteFuture,
  ReadRemoteFuture: Future<Output = Result<Vec<u8>, TransportError>>,
{
  let bytes = match classify_schema_location(environment, &url)? {
    SchemaLocation::File(path) => read_file(path).await?,
    SchemaLocation::Remote => read_remote(url.clone()).await?,
  };
  decode_json(url, &bytes)
}

/// Adapt one environment byte read into the schema transport error model.
async fn read_bytes_with<ReadFile, ReadFuture>(path: PathBuf, read_file: ReadFile) -> Result<Vec<u8>, TransportError>
where
  ReadFile: FnOnce(PathBuf) -> ReadFuture,
  ReadFuture: Future<Output = Result<Vec<u8>, EnvironmentError>>,
{
  read_file(path).await.map_err(TransportError::from)
}

/// Adapt one environment byte write into the schema transport error model.
async fn write_bytes_with<WriteFile, WriteFuture>(path: PathBuf, bytes: Vec<u8>, write_file: WriteFile) -> Result<(), TransportError>
where
  WriteFile: FnOnce(PathBuf, Vec<u8>) -> WriteFuture,
  WriteFuture: Future<Output = Result<(), EnvironmentError>>,
{
  write_file(path, bytes).await.map_err(TransportError::from)
}

/// Construct the browser/current-thread HTTP client used by local schema transport.
///
/// # Errors
///
/// Returns [`TransportError`] when required TLS initialization or client
/// construction fails.
#[cfg(feature = "reqwest")]
pub fn local_http_client() -> Result<reqwest::Client, TransportError> {
  build_http_client(reqwest::Client::builder())
}

/// Complete one HTTP client builder through the shared typed TLS boundary.
#[cfg(feature = "reqwest")]
fn build_http_client(builder: reqwest::ClientBuilder) -> Result<reqwest::Client, TransportError> {
  #[cfg(feature = "rustls-tls")]
  if CryptoProvider::get_default().is_none() && default_provider().install_default().is_err() && CryptoProvider::get_default().is_none() {
    return Err(TransportError::CryptoProvider);
  }

  builder.build().map_err(|source| TransportError::HttpClient {
    source: Box::new(source)
  })
}

/// Construct the native HTTP client used by concurrent schema transport.
///
/// The `TAPLO_EXTRA_CA_CERTS` environment variable may name one PEM or DER
/// certificate to add to the selected TLS implementation.
///
/// # Errors
///
/// Returns [`TransportError`] when client construction, custom-certificate
/// loading, certificate decoding, or required TLS initialization fails.
#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
#[allow(
  clippy::single_call_fn,
  reason = "the native HTTP factory owns TLS-provider setup, custom trust roots, and transport timeout policy"
)]
pub fn concurrent_http_client(environment: &impl Environment, timeout: Duration) -> Result<reqwest::Client, TransportError> {
  #[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
  /// Add one configured PEM or DER certificate to the client builder.
  #[allow(
    clippy::single_call_fn,
    reason = "certificate loading isolates PEM-or-DER decoding and root insertion from HTTP client configuration"
  )]
  fn add_certificate(builder: reqwest::ClientBuilder, configured_path: &str) -> Result<reqwest::ClientBuilder, TransportError> {
    let certificate_path = Path::new(configured_path);
    let bytes = fs::read(certificate_path).map_err(|source| TransportError::CertificateRead {
      path: certificate_path.into(),
      source,
    })?;
    let certificate = if certificate_path.extension().is_some_and(|extension| extension == "der") {
      reqwest::Certificate::from_der(&bytes)
    } else {
      reqwest::Certificate::from_pem(&bytes)
    }
    .map_err(|source| TransportError::CertificateDecode {
      path:   certificate_path.into(),
      source: Box::new(source),
    })?;
    Ok(builder.add_root_certificate(certificate))
  }

  #[cfg(not(any(feature = "native-tls", feature = "rustls-tls")))]
  /// Reject a configured certificate when no TLS implementation is enabled.
  #[allow(
    clippy::single_call_fn,
    reason = "the no-TLS certificate adapter preserves the client-building contract while returning a typed capability error"
  )]
  fn add_certificate(_builder: reqwest::ClientBuilder, path: &str) -> Result<reqwest::ClientBuilder, TransportError> {
    Err(TransportError::TlsUnavailable {
      path: PathBuf::from(path)
    })
  }

  let mut builder = reqwest::Client::builder().timeout(timeout);
  if let Some(path) = environment.env_var("TAPLO_EXTRA_CA_CERTS")? {
    builder = add_certificate(builder, &path)?;
  }
  build_http_client(builder)
}

/// Static transport interface used by schema interpretation and caching.
pub trait SchemaTransport: Clone + 'static {
  /// Future returned by [`Self::read_bytes`].
  type ReadBytesFuture<'a>: Future<Output = Result<Vec<u8>, TransportError>> + 'a
  where
    Self: 'a;
  /// Future returned by [`Self::read_json`].
  type ReadFuture<'a>: Future<Output = Result<Value, TransportError>> + 'a
  where
    Self: 'a;
  /// Future returned by [`Self::write_bytes`].
  type WriteFuture<'a>: Future<Output = Result<(), TransportError>> + 'a
  where
    Self: 'a;

  /// Return the current host time.
  ///
  /// # Errors
  ///
  /// Returns [`TransportError`] when the host clock is unavailable or malformed.
  fn now(&self) -> Result<OffsetDateTime, TransportError>;

  /// Return whether one path is absolute in the host path model.
  ///
  /// # Errors
  ///
  /// Returns [`TransportError`] when the host path callback fails.
  fn is_absolute(&self, path: &Path) -> Result<bool, TransportError>;

  /// Read one host file as bytes.
  ///
  /// # Errors
  ///
  /// Returns [`TransportError`] when the host read fails.
  fn read_bytes(&self, path: PathBuf) -> Self::ReadBytesFuture<'_>;

  /// Load one JSON document from a supported URL.
  ///
  /// # Errors
  ///
  /// Returns [`TransportError`] when the URL scheme is unsupported, transport
  /// is unavailable, I/O fails, or the returned bytes are not valid JSON.
  fn read_json(&self, url: Url) -> Self::ReadFuture<'_>;

  /// Atomically persist one byte buffer to a host path.
  ///
  /// # Errors
  ///
  /// Returns [`TransportError`] when the host write fails.
  fn write_bytes(&self, path: PathBuf, bytes: Vec<u8>) -> Self::WriteFuture<'_>;
}

/// Schema transport whose borrowed operations can cross thread boundaries.
///
/// This refines [`SchemaTransport`] without changing the local-capable base
/// contract. Any transport with thread-safe state and `Send` read/write futures
/// implements the capability automatically.
pub trait ConcurrentTransport: SchemaTransport + Send + Sync
where
  for<'transport> Self::ReadBytesFuture<'transport>: Send,
  for<'transport> Self::ReadFuture<'transport>: Send,
  for<'transport> Self::WriteFuture<'transport>: Send,
{
}

impl<T> ConcurrentTransport for T
where
  T: SchemaTransport + Send + Sync,
  for<'transport> T::ReadBytesFuture<'transport>: Send,
  for<'transport> T::ReadFuture<'transport>: Send,
  for<'transport> T::WriteFuture<'transport>: Send,
{
}

/// Define one public online transport façade over an environment and HTTP client.
#[cfg(feature = "reqwest")]
macro_rules! define_online_transport {
  ($name:ident, $environment:ident, $description:literal, $environment_description:literal) => {
    #[doc = $description]
    #[derive(Clone, Debug)]
    pub struct $name<E: $environment> {
      /// Host capabilities for this execution model.
      environment: E,
      /// HTTP client compatible with this execution model.
      http:        reqwest::Client,
    }

    impl<E: $environment> $name<E> {
      /// Construct an online schema transport.
      #[must_use]
      pub const fn new(environment: E, http: reqwest::Client) -> Self {
        Self {
          environment,
          http,
        }
      }

      #[doc = $environment_description]
      #[must_use]
      pub const fn environment(&self) -> &E {
        &self.environment
      }
    }
  };
}

#[cfg(feature = "reqwest")]
define_online_transport!(
  LocalSchemaTransport,
  LocalEnvironment,
  "Browser/current-thread transport backed by a [`LocalEnvironment`].",
  "Borrow the underlying local environment."
);

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
define_online_transport!(
  ConcurrentSchemaTransport,
  ConcurrentEnvironment,
  "Native multi-thread transport backed by a [`ConcurrentEnvironment`].",
  "Borrow the underlying concurrent environment."
);

/// Shared state access required by a local-capable schema transport.
trait LocalTransportState: Clone + 'static {
  /// Local host capability type.
  type Environment: LocalEnvironment;

  /// Borrow the local host capabilities.
  fn local_environment(&self) -> &Self::Environment;

  /// Read one remote schema through this transport's local execution model.
  fn read_remote_local(&self, url: Url) -> LocalBoxFuture<'_, Result<Vec<u8>, TransportError>>;
}

#[cfg(feature = "reqwest")]
impl<E: LocalEnvironment> LocalTransportState for LocalSchemaTransport<E> {
  type Environment = E;

  fn local_environment(&self) -> &Self::Environment {
    &self.environment
  }

  fn read_remote_local(&self, url: Url) -> LocalBoxFuture<'_, Result<Vec<u8>, TransportError>> {
    read_http_local(&self.http, url)
  }
}

impl<T: LocalTransportState> SchemaTransport for T {
  type ReadBytesFuture<'a>
    = LocalBoxFuture<'a, Result<Vec<u8>, TransportError>>
  where
    Self: 'a;
  type ReadFuture<'a>
    = LocalBoxFuture<'a, Result<Value, TransportError>>
  where
    Self: 'a;
  type WriteFuture<'a>
    = LocalBoxFuture<'a, Result<(), TransportError>>
  where
    Self: 'a;

  fn now(&self) -> Result<OffsetDateTime, TransportError> {
    current_time(self.local_environment())
  }

  fn is_absolute(&self, path: &Path) -> Result<bool, TransportError> {
    path_is_absolute(self.local_environment(), path)
  }

  fn read_bytes(&self, path: PathBuf) -> Self::ReadBytesFuture<'_> {
    let environment = self.local_environment();
    read_bytes_with(path, move |file_path| async move { environment.read_file(&file_path).await }).boxed_local()
  }

  fn read_json(&self, url: Url) -> Self::ReadFuture<'_> {
    let environment = self.local_environment();
    read_json_with(
      environment,
      url,
      move |file_path| async move { environment.read_file(&file_path).await },
      move |remote_url| self.read_remote_local(remote_url),
    )
    .boxed_local()
  }

  fn write_bytes(&self, path: PathBuf, bytes: Vec<u8>) -> Self::WriteFuture<'_> {
    let environment = self.local_environment();
    write_bytes_with(path, bytes, move |file_path, file_bytes| async move {
      environment.write_file(&file_path, &file_bytes).await
    })
    .boxed_local()
  }
}

#[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
impl<E: ConcurrentEnvironment> SchemaTransport for ConcurrentSchemaTransport<E> {
  type ReadBytesFuture<'a>
    = BoxFuture<'a, Result<Vec<u8>, TransportError>>
  where
    Self: 'a;
  type ReadFuture<'a>
    = BoxFuture<'a, Result<Value, TransportError>>
  where
    Self: 'a;
  type WriteFuture<'a>
    = BoxFuture<'a, Result<(), TransportError>>
  where
    Self: 'a;

  fn now(&self) -> Result<OffsetDateTime, TransportError> {
    current_time(&self.environment)
  }

  fn is_absolute(&self, path: &Path) -> Result<bool, TransportError> {
    path_is_absolute(&self.environment, path)
  }

  fn read_bytes(&self, path: PathBuf) -> Self::ReadBytesFuture<'_> {
    read_bytes_with(path, move |file_path| self.environment.read_file_concurrent(file_path)).boxed()
  }

  fn read_json(&self, url: Url) -> Self::ReadFuture<'_> {
    read_json_with(
      &self.environment,
      url,
      move |file_path| self.environment.read_file_concurrent(file_path),
      move |remote_url| read_http_concurrent(&self.http, remote_url),
    )
    .boxed()
  }

  fn write_bytes(&self, path: PathBuf, bytes: Vec<u8>) -> Self::WriteFuture<'_> {
    write_bytes_with(path, bytes, move |file_path, file_bytes| {
      self.environment.write_file_concurrent(file_path, file_bytes)
    })
    .boxed()
  }
}

/// Offline transport retaining built-in, memory, disk-cache, and local-file access.
#[derive(Clone, Debug)]
pub struct OfflineSchemaTransport<E: LocalEnvironment> {
  /// Local host capabilities.
  environment: E,
}

impl<E: LocalEnvironment> OfflineSchemaTransport<E> {
  /// Construct an offline transport.
  #[must_use]
  #[allow(
    clippy::single_call_fn,
    reason = "the public offline-transport constructor establishes an explicit no-network schema capability boundary"
  )]
  pub const fn new(environment: E) -> Self {
    Self {
      environment,
    }
  }

  /// Borrow the underlying environment.
  #[must_use]
  pub const fn environment(&self) -> &E {
    &self.environment
  }
}

impl<E: LocalEnvironment> LocalTransportState for OfflineSchemaTransport<E> {
  type Environment = E;

  fn local_environment(&self) -> &Self::Environment {
    &self.environment
  }

  fn read_remote_local(&self, url: Url) -> LocalBoxFuture<'_, Result<Vec<u8>, TransportError>> {
    async move {
      Err(TransportError::RemoteUnavailable {
        url,
      })
    }
    .boxed_local()
  }
}

#[cfg(test)]
mod tests {
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use std::path::Path;
  use std::path::PathBuf;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use std::time::Duration;

  use futures::FutureExt as _;
  use futures::executor::block_on;
  use futures::future::LocalBoxFuture;
  use serde_json::json;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  use taplo_test_support::ensure_result;
  use url::Url;

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use super::ConcurrentSchemaTransport;
  #[cfg(feature = "reqwest")]
  use super::LocalSchemaTransport;
  use super::OfflineSchemaTransport;
  use super::SchemaTransport;
  use super::TransportError;
  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  use crate::environment::Environment as _;
  use crate::test_support::TestEnvironment;
  /// Parse one transport fixture URL.
  fn url(input: &str) -> Result<Url, TestFailure> {
    ensure_ok(Url::parse(input), "the transport fixture URL must parse")
  }

  /// Exercise the file, decoder, and scheme contracts shared by every transport.
  fn file_transport_contract<T: SchemaTransport>(transport: &T) -> LocalBoxFuture<'_, Result<(), TestFailure>> {
    async move {
      let schema = ensure_result(
        transport.read_json(url("file:///workspace/schema.json")?).await,
        "file-schema loading must succeed",
      )?;
      let title = ensure_some(schema.get("title"), "the file schema must retain its title")?;
      ensure_eq(title, &json!("local"), "file transport must preserve the decoded JSON value")?;

      let invalid = ensure_some(
        transport.read_json(url("file:///workspace/invalid.json")?).await.err(),
        "invalid file JSON must fail",
      )?;
      ensure(
        matches!(invalid, TransportError::Json { .. }),
        "invalid file JSON must retain its transport URL and decoder error",
      )?;

      let invalid_url = ensure_some(
        transport.read_json(url("file://remote-host/schema.json")?).await.err(),
        "a nonlocal file URL must fail",
      )?;
      ensure(
        matches!(invalid_url, TransportError::InvalidFileUrl { .. }),
        "a nonlocal file URL must retain the typed file-path boundary",
      )?;

      let unsupported = ensure_some(
        transport.read_json(url("ftp://example.com/schema.json")?).await.err(),
        "an unsupported transport scheme must fail",
      )?;
      ensure(
        matches!(
          unsupported,
          TransportError::UnsupportedScheme {
            ref scheme,
            ..
          } if scheme == "ftp"
        ),
        "the unsupported-scheme error must retain the rejected scheme",
      )
    }
    .boxed_local()
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Loopback server thread used by one HTTP transport fixture.
  type LoopbackServer = std::thread::JoinHandle<Result<(), std::io::Error>>;

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Endpoint and server thread for one loopback HTTP response.
  type LoopbackFixture = (Url, LoopbackServer);

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Start one loopback server that writes the supplied complete HTTP response.
  fn serve_once(response: &'static [u8]) -> Result<LoopbackFixture, TestFailure> {
    let listener = ensure_ok(
      std::net::TcpListener::bind(("127.0.0.1", 0)),
      "the loopback schema listener must bind",
    )?;
    let address = ensure_ok(listener.local_addr(), "the loopback schema listener must expose its address")?;
    let server = std::thread::spawn(move || {
      let (mut stream, _) = listener.accept()?;
      let mut request = [0_u8; 1024];
      if std::io::Read::read(&mut stream, &mut request)? == 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
      }
      std::io::Write::write_all(&mut stream, response)
    });
    let endpoint = ensure_ok(
      Url::parse(&format!("http://{address}/schema.json")),
      "the loopback schema URL must parse",
    )?;
    Ok((endpoint, server))
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Join one loopback server and preserve its typed I/O result.
  fn finish_server(server: LoopbackServer) -> Result<(), TestFailure> {
    let joined = server.join();
    ensure(joined.is_ok(), "the panic-free loopback schema server must terminate normally")?;
    ensure_result(
      ensure_some(joined.ok(), "the completed loopback server must retain its I/O result")?,
      "the loopback schema server must write its response",
    )
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Read one loopback response through a real schema transport.
  fn read_http_once<T: SchemaTransport>(
    runtime: &tokio::runtime::Runtime,
    transport: &T,
    response: &'static [u8],
  ) -> Result<Result<serde_json::Value, TransportError>, TestFailure> {
    let (endpoint, server) = serve_once(response)?;
    let result = runtime.block_on(transport.read_json(endpoint));
    finish_server(server)?;
    Ok(result)
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Read one failing response through both online transport capabilities.
  fn http_error_pair<L: SchemaTransport, C: SchemaTransport>(
    runtime: &tokio::runtime::Runtime,
    local: &L,
    concurrent: &C,
    response: &'static [u8],
  ) -> Result<(TransportError, TransportError), TestFailure> {
    let local_error = ensure_some(
      read_http_once(runtime, local, response)?.err(),
      "the local transport must reject the failing HTTP response",
    )?;
    let concurrent_error = ensure_some(
      read_http_once(runtime, concurrent, response)?.err(),
      "the concurrent transport must reject the failing HTTP response",
    )?;
    Ok((local_error, concurrent_error))
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  /// Require both online transport families to preserve the typed HTTP failure boundary.
  fn ensure_http_failure_pair(errors: (TransportError, TransportError), context: &'static str) -> Result<(), TestFailure> {
    ensure(
      [
        matches!(errors.0, TransportError::Http { .. }),
        matches!(errors.1, TransportError::Http { .. }),
      ] == [true, true],
      context,
    )
  }

  #[test]
  fn offline_transport_preserves_local_io_and_typed_scheme_boundaries() -> Result<(), TestFailure> {
    block_on(async {
      let environment = TestEnvironment::default();
      let local_schema = ensure_ok(
        serde_json::to_vec(&json!({ "title": "local" })),
        "the local schema fixture must serialize",
      )?;
      environment.insert_file("/workspace/schema.json", local_schema);
      environment.insert_file("/workspace/invalid.json", b"not-json".to_vec());
      let transport = OfflineSchemaTransport::new(environment.clone());

      file_transport_contract(&transport).await?;

      let remote = ensure_some(
        transport.read_json(url("https://example.com/schema.json")?).await.err(),
        "offline HTTP loading must fail",
      )?;
      ensure(
        matches!(remote, TransportError::RemoteUnavailable { .. }),
        "offline HTTP failure must be distinguishable from an unsupported scheme",
      )?;

      let output = PathBuf::from("/workspace/cache/schema");
      ensure_result(
        transport.write_bytes(output.clone(), b"cached".to_vec()).await,
        "offline cache writes must retain local host capability",
      )?;
      let written = ensure_result(
        transport.read_bytes(output).await,
        "offline cache writes must be readable through the same capability",
      )?;
      ensure(
        written.as_slice() == b"cached",
        "offline byte transport must preserve exact contents",
      )
    })
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn online_transports_share_file_and_http_success_error_contracts() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    environment.insert_file(
      "/workspace/schema.json",
      ensure_ok(
        serde_json::to_vec(&json!({ "title": "local" })),
        "the online file-schema fixture must serialize",
      )?,
    );
    environment.insert_file("/workspace/invalid.json", b"not-json".to_vec());
    let local = LocalSchemaTransport::new(
      environment.clone(),
      ensure_result(super::local_http_client(), "the local HTTP schema client must construct")?,
    );
    let concurrent = ConcurrentSchemaTransport::new(
      environment.clone(),
      ensure_result(
        super::concurrent_http_client(&environment, Duration::from_secs(2)),
        "the concurrent HTTP schema client must construct",
      )?,
    );
    block_on(file_transport_contract(&local))?;
    block_on(file_transport_contract(&concurrent))?;

    let runtime = ensure_ok(
      tokio::runtime::Builder::new_current_thread().enable_all().build(),
      "the loopback schema runtime must construct",
    )?;
    let success = b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nConnection: close\r\n\r\n{\"title\":\"loop\"}";
    let local_schema = ensure_result(
      read_http_once(&runtime, &local, success)?,
      "the local transport must decode a successful loopback response",
    )?;
    let concurrent_schema = ensure_result(
      read_http_once(&runtime, &concurrent, success)?,
      "the concurrent transport must decode a successful loopback response",
    )?;
    ensure_eq(
      &local_schema,
      &json!({ "title": "loop" }),
      "the local HTTP transport must preserve the response JSON",
    )?;
    ensure_eq(
      &concurrent_schema,
      &local_schema,
      "local and concurrent HTTP transports must decode the same successful response",
    )?;

    let status = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    ensure_http_failure_pair(
      http_error_pair(&runtime, &local, &concurrent, status)?,
      "local and concurrent status failures must retain the same typed HTTP family",
    )?;

    let truncated = b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n{\"title\":";
    ensure_http_failure_pair(
      http_error_pair(&runtime, &local, &concurrent, truncated)?,
      "local and concurrent body-read failures must retain the same typed HTTP family",
    )
  }

  #[cfg(all(feature = "reqwest", not(target_arch = "wasm32")))]
  #[test]
  fn concurrent_client_reads_custom_certificate_from_its_environment() -> Result<(), TestFailure> {
    let environment = TestEnvironment::default();
    environment.set_env_var("TAPLO_EXTRA_CA_CERTS", "/missing/taplo-ca.pem");
    let error = ensure_some(
      super::concurrent_http_client(&environment, Duration::from_secs(1)).err(),
      "a configured missing certificate must prevent client construction",
    )?;
    #[cfg(any(feature = "native-tls", feature = "rustls-tls"))]
    ensure(
      matches!(
        error,
        TransportError::CertificateRead {
          ref path,
          ..
        } if path.as_path() == Path::new("/missing/taplo-ca.pem")
      ),
      "a TLS-enabled client must retain the environment-provided certificate path",
    )?;
    #[cfg(not(any(feature = "native-tls", feature = "rustls-tls")))]
    ensure(
      matches!(
        error,
        TransportError::TlsUnavailable {
          ref path
        } if path.as_path() == Path::new("/missing/taplo-ca.pem")
      ),
      "a client without TLS must reject the environment-provided certificate path",
    )?;
    let configured = ensure_result(
      environment.env_var("TAPLO_EXTRA_CA_CERTS"),
      "the certificate variable must remain readable",
    )?;
    ensure(
      configured.as_deref() == Some("/missing/taplo-ca.pem"),
      "client construction must not mutate the owning host environment",
    )
  }
}
