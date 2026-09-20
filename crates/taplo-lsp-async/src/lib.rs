//! Local and concurrent JSON-RPC/LSP server runtimes over shared protocol transitions.

pub mod rpc;
pub mod util;

mod runtime;

#[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
mod listen;

use std::cell::Cell;
use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::TryReserveError;
use std::collections::hash_map::Entry;
use std::fmt;
use std::io;
use std::iter::successors;
use std::mem;
use std::num::ParseIntError;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::Context;
use std::task::Poll;
use std::task::Waker;

use futures::Future;
use futures::FutureExt as _;
use futures::channel::mpsc::SendError;
use futures::channel::oneshot;
use futures::future::FusedFuture;
use lsp_types::NumberOrString;
use lsp_types::notification as notification_types;
use lsp_types::notification::Notification as _;
use lsp_types::request as request_types;
use lsp_types::request::Request as _;
use parking_lot::Mutex;
pub use runtime::ConcurrentContext;
pub use runtime::ConcurrentMessageWriter;
pub use runtime::ConcurrentServer;
pub use runtime::LocalContext;
pub use runtime::LocalMessageWriter;
pub use runtime::LocalServer;
use serde::Serialize;
#[cfg(test)]
use serde::ser::Error as SerializeError;
use thiserror::Error;
#[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
use tokio::runtime::TryCurrentError;
#[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
use tokio::task::JoinError;

/// Failure produced by a message-writer port.
#[derive(Debug, Error)]
pub enum MessageWriterError {
  /// The underlying stream or host writer failed.
  #[error("message writer I/O failed: {source}")]
  Io {
    /// Typed stream or host I/O failure.
    #[source]
    source: Box<io::Error>,
  },
  /// A native transport's serialized output channel closed before delivery.
  #[error("LSP output channel closed before message delivery: {source}")]
  OutputChannelClosed {
    /// Futures channel's typed send failure.
    #[source]
    source: Box<SendError>,
  },
}

impl MessageWriterError {
  /// Return the stable I/O category represented by this writer failure.
  #[must_use]
  pub fn kind(&self) -> io::ErrorKind {
    match *self {
      Self::Io {
        ref source,
      } => source.kind(),
      Self::OutputChannelClosed {
        ..
      } => io::ErrorKind::BrokenPipe,
    }
  }
}

impl From<io::Error> for MessageWriterError {
  fn from(source: io::Error) -> Self {
    Self::Io {
      source: Box::new(source)
    }
  }
}

/// Complete a no-buffer message-writer flush or close operation immediately.
///
/// JSON-RPC writers that deliver complete messages may share this readiness contract while
/// retaining their own send and failure behavior.
pub const fn message_writer_ready<WriterError>() -> Poll<Result<(), WriterError>> {
  Poll::Ready(Ok(()))
}

/// Implement immediate flush and close readiness for a discrete message writer.
///
/// Invoke this macro inside a [`Sink`](futures::Sink) implementation whose messages are delivered
/// atomically by `start_send`. The writer retains ownership of readiness, delivery, and error
/// behavior while this shared contract supplies the two no-buffer lifecycle methods.
#[macro_export]
macro_rules! implement_message_writer_readiness {
  () => {
    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
      $crate::message_writer_ready()
    }

    fn poll_close(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
      $crate::message_writer_ready()
    }
  };
}

/// Result of invoking an erased request handler before its JSON-RPC response is written.
#[derive(Debug)]
enum RequestOutcome {
  /// Typed handler success serialized to JSON.
  Success(serde_json::Value),
  /// Typed handler failure already represented as an RPC error.
  RpcError(rpc::RpcError),
  /// Request parameters could not be deserialized.
  InvalidParams(String),
  /// A successful typed result could not be serialized.
  SerializationFailure(String),
}

/// Test value that deliberately rejects JSON serialization at protocol boundaries.
#[cfg(test)]
struct SerializationFailureFixture;

#[cfg(test)]
impl Serialize for SerializationFailureFixture {
  fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
  where
    S: serde::Serializer,
  {
    Err(<S::Error as SerializeError>::custom("fixture value cannot be serialized"))
  }
}

/// A server-runtime failure outside an individual JSON-RPC request result.
#[derive(Debug, Error)]
pub enum ServerError {
  /// A native transport was started outside an active Tokio runtime.
  #[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
  #[error("native LSP transport requires an active Tokio runtime: {source}")]
  RuntimeUnavailable {
    /// Tokio's typed runtime-discovery failure.
    #[source]
    source: TryCurrentError,
  },
  /// A message-writer port failed.
  #[error("message transport failed: {0}")]
  Transport(#[from] MessageWriterError),
  /// A native stream or socket operation failed.
  #[error("transport I/O failed: {0}")]
  Io(#[from] io::Error),
  /// A native transport task terminated without returning its typed result.
  #[cfg(any(feature = "tokio-stdio", feature = "tokio-tcp"))]
  #[error("native LSP {task} task terminated unexpectedly: {source}")]
  TransportTaskTerminated {
    /// Stable role of the failed task.
    task:   &'static str,
    /// Tokio's typed task-termination failure.
    #[source]
    source: JoinError,
  },
  /// Typed message serialization failed.
  #[error("failed to serialize {context}: {source}")]
  Serialization {
    /// The value being serialized.
    context: &'static str,
    /// The serialization failure.
    source:  serde_json::Error,
  },
  /// Typed response deserialization failed.
  #[error("failed to deserialize an outbound response: {0}")]
  ResponseDeserialization(serde_json::Error),
  /// A malformed client response could not be decoded and must not receive a response.
  #[error(transparent)]
  MalformedClientResponse(#[from] rpc::InvalidResponseDecode),
  /// An LSP transport header is malformed.
  #[error("malformed LSP transport header: {header}")]
  MalformedHeader {
    /// The rejected header without its line terminator.
    header: String,
  },
  /// An LSP message omits its required content-length header.
  #[error("LSP message omits the Content-Length header")]
  MissingContentLength,
  /// An LSP transport ended after beginning a header block but before its terminating blank line.
  #[error("LSP transport ended before the current header block was complete")]
  IncompleteHeader,
  /// An LSP content length is malformed.
  #[error("invalid LSP Content-Length value {header_value}: {source}")]
  InvalidContentLength {
    /// The rejected header value.
    header_value: String,
    /// The integer parsing failure.
    source:       ParseIntError,
  },
  /// An LSP message declares its content length more than once.
  #[error("LSP message declares Content-Length more than once: {first} and {second}")]
  DuplicateContentLength {
    /// The first declared content length.
    first:  usize,
    /// The repeated content length.
    second: usize,
  },
  /// An LSP message exceeds the transport's declared safety limit.
  #[error("LSP message length {length} exceeds the {maximum}-byte transport limit")]
  MessageTooLarge {
    /// The declared message length.
    length:  usize,
    /// The accepted maximum.
    maximum: usize,
  },
  /// A message buffer could not reserve its declared length.
  #[error("failed to reserve {length} bytes for an LSP message: {source}")]
  MessageAllocation {
    /// The declared message length.
    length: usize,
    /// The typed allocation failure.
    #[source]
    source: TryReserveError,
  },
  /// The client sent `exit` before completing shutdown.
  #[error("received exit before a successful shutdown response")]
  ExitBeforeShutdown,
  /// A pending outbound request lost its response channel.
  #[error("outbound request response channel was dropped")]
  ResponseChannelClosed,
  /// A client response uses an unsupported JSON-RPC version.
  #[error("response {id:?} uses unsupported JSON-RPC version `{version}`")]
  InvalidResponseVersion {
    /// Response identifier.
    id:      rpc::RequestId,
    /// Rejected version.
    version: String,
  },
  /// A client response contains both or neither of the result and error channels.
  #[error("response {id:?} must contain exactly one result or error channel")]
  InvalidResponseShape {
    /// Response identifier.
    id: rpc::RequestId,
  },
  /// The outbound numeric request-ID space is exhausted.
  #[error("outbound request ID space is exhausted")]
  RequestIdExhausted,
  /// The mutation ordering sequence is exhausted.
  #[error("mutation ordering sequence is exhausted")]
  MutationSequenceExhausted,
}

/// Cancellation behavior required by the shared session transition model.
trait CancellationState: Clone + Default {
  /// Runtime-specific token observing this state.
  type Token: Clone;

  /// Construct a token observing this state.
  fn token(&self) -> Self::Token;

  /// Mark the task cancelled and wake its waiter if registered.
  fn cancel(&self);
}

/// Runtime-specific storage for one independently pollable task waker.
trait WakerRegistration: Clone + Default + fmt::Debug {
  /// Replace the currently registered task waker.
  fn replace(&self, waker: &Waker);

  /// Remove and return the currently registered task waker.
  fn take(&self) -> Option<Waker>;

  /// Remove any registered task waker without waking it.
  fn clear(&self) {
    drop(self.take());
  }

  /// Return whether two handles identify the same registration.
  fn is_same(&self, other: &Self) -> bool;
}

/// Current-thread task-waker registration.
#[derive(Clone, Default)]
struct LocalWakerRegistration(Rc<Cell<Option<Waker>>>);

impl fmt::Debug for LocalWakerRegistration {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("LocalWakerRegistration").finish_non_exhaustive()
  }
}

impl WakerRegistration for LocalWakerRegistration {
  fn replace(&self, waker: &Waker) {
    self.0.set(Some(waker.clone()));
  }

  fn take(&self) -> Option<Waker> {
    self.0.take()
  }

  fn is_same(&self, other: &Self) -> bool {
    Rc::ptr_eq(&self.0, &other.0)
  }
}

/// Thread-safe task-waker registration.
#[derive(Clone, Debug, Default)]
struct ConcurrentWakerRegistration(Arc<Mutex<Option<Waker>>>);

impl WakerRegistration for ConcurrentWakerRegistration {
  fn replace(&self, waker: &Waker) {
    *self.0.lock() = Some(waker.clone());
  }

  fn take(&self) -> Option<Waker> {
    self.0.lock().take()
  }

  fn is_same(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.0, &other.0)
  }
}

/// Runtime-specific collection of cancellation-token registrations.
trait CancellationRegistryStorage: Clone + Default + fmt::Debug {
  /// Registration representation used by this execution model.
  type Registration: WakerRegistration;

  /// Add one independently pollable token registration.
  fn register(&self) -> Self::Registration;

  /// Remove one dropped token registration.
  fn remove(&self, registration: &Self::Registration);

  /// Drain every registration in preparation for cancellation wakeup.
  fn drain(&self) -> Vec<Self::Registration>;
}

/// Current-thread cancellation registry.
#[derive(Clone, Default)]
struct LocalCancellationRegistry(Rc<Cell<Vec<LocalWakerRegistration>>>);

impl fmt::Debug for LocalCancellationRegistry {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("LocalCancellationRegistry").finish_non_exhaustive()
  }
}

impl CancellationRegistryStorage for LocalCancellationRegistry {
  type Registration = LocalWakerRegistration;

  fn register(&self) -> Self::Registration {
    let registration = LocalWakerRegistration::default();
    let mut registrations = self.0.take();
    registrations.push(registration.clone());
    self.0.set(registrations);
    registration
  }

  fn remove(&self, registration: &Self::Registration) {
    let mut registrations = self.0.take();
    registrations.retain(|registered| !registered.is_same(registration));
    self.0.set(registrations);
  }

  fn drain(&self) -> Vec<Self::Registration> {
    self.0.take()
  }
}

/// Thread-safe cancellation registry.
#[derive(Clone, Debug, Default)]
struct ConcurrentCancellationRegistry(Arc<Mutex<Vec<ConcurrentWakerRegistration>>>);

impl CancellationRegistryStorage for ConcurrentCancellationRegistry {
  type Registration = ConcurrentWakerRegistration;

  fn register(&self) -> Self::Registration {
    let registration = ConcurrentWakerRegistration::default();
    self.0.lock().push(registration.clone());
    registration
  }

  fn remove(&self, registration: &Self::Registration) {
    self.0.lock().retain(|registered| !registered.is_same(registration));
  }

  fn drain(&self) -> Vec<Self::Registration> {
    mem::take(&mut *self.0.lock())
  }
}

/// Stable waker registration owned by one cancellation-token clone.
#[derive(Debug)]
struct CancellationWaiter<S: CancellationRegistryStorage> {
  /// Registry notified by the corresponding cancellation state.
  registry:     S,
  /// Current waker for this independently pollable token.
  registration: S::Registration,
}

impl<S: CancellationRegistryStorage> CancellationWaiter<S> {
  /// Register one independently pollable cancellation token.
  fn register(registry: S) -> Self {
    let registration = registry.register();
    Self {
      registry,
      registration,
    }
  }

  /// Replace this token's current task waker.
  fn update(&self, waker: &Waker) {
    self.registration.replace(waker);
  }
}

impl<S: CancellationRegistryStorage> Clone for CancellationWaiter<S> {
  fn clone(&self) -> Self {
    Self::register(self.registry.clone())
  }
}

impl<S: CancellationRegistryStorage> Drop for CancellationWaiter<S> {
  fn drop(&mut self) {
    self.registry.remove(&self.registration);
    self.registration.clear();
  }
}

/// Poll one cancellation flag without losing a request racing waker registration.
fn poll_cancellation<S, F>(waiter: &CancellationWaiter<S>, context: &Context<'_>, is_cancelled: F) -> Poll<()>
where
  S: CancellationRegistryStorage,
  F: Fn() -> bool,
{
  if is_cancelled() {
    return Poll::Ready(());
  }
  waiter.update(context.waker());
  if is_cancelled() { Poll::Ready(()) } else { Poll::Pending }
}

/// Adapt a cancellation token's completion into the LSP request-cancelled error.
fn poll_cancellation_error<T>(token: &mut T, context: &mut Context<'_>) -> Poll<Result<(), rpc::RpcError>>
where
  T: Future<Output = ()> + Unpin,
{
  match token.poll_unpin(context) {
    Poll::Ready(()) => Poll::Ready(Err(rpc::RpcError::request_cancelled())),
    Poll::Pending => Poll::Pending,
  }
}

/// Wake every live cancellation-token registration after releasing registry state.
fn wake_all<S: CancellationRegistryStorage>(registry: &S) {
  let wakers = registry
    .drain()
    .into_iter()
    .filter_map(|registration| registration.take())
    .collect::<Vec<_>>();
  for waker in wakers {
    waker.wake();
  }
}

/// Local cancellation state using only current-thread shared ownership.
#[derive(Debug, Clone, Default)]
struct LocalCancellation {
  /// Whether cancellation was requested.
  cancelled: Rc<Cell<bool>>,
  /// Independently pollable token registrations.
  registry:  LocalCancellationRegistry,
}

impl CancellationState for LocalCancellation {
  type Token = LocalCancelToken;

  fn token(&self) -> Self::Token {
    LocalCancelToken {
      cancelled: Rc::clone(&self.cancelled),
      waiter:    CancellationWaiter::register(self.registry.clone()),
    }
  }

  fn cancel(&self) {
    self.cancelled.set(true);
    wake_all(&self.registry);
  }
}

/// Concurrent cancellation state using thread-safe shared ownership.
#[derive(Debug, Clone, Default)]
struct ConcurrentCancellation {
  /// Whether cancellation was requested.
  cancelled: Arc<AtomicBool>,
  /// Independently pollable token registrations.
  registry:  ConcurrentCancellationRegistry,
}

impl CancellationState for ConcurrentCancellation {
  type Token = ConcurrentCancelToken;

  fn token(&self) -> Self::Token {
    ConcurrentCancelToken {
      cancelled: Arc::clone(&self.cancelled),
      waiter:    CancellationWaiter::register(self.registry.clone()),
    }
  }

  fn cancel(&self) {
    self.cancelled.store(true, Ordering::SeqCst);
    wake_all(&self.registry);
  }
}

/// Runtime-specific storage for shared ordered-mutation transitions.
trait MutationBarrierStorage: Clone + Default {
  /// Waker registration used by this execution model.
  type Registration: WakerRegistration;

  /// Mutate the barrier state synchronously and return the closure result.
  fn with_state<R>(&self, operation: impl FnOnce(&mut MutationBarrierState<Self::Registration>) -> R) -> R;
}

/// Current-thread mutation storage for [`LocalServer`].
#[derive(Clone, Default)]
struct LocalMutationStorage(Rc<Cell<MutationBarrierState<LocalWakerRegistration>>>);

impl fmt::Debug for LocalMutationStorage {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("LocalMutationStorage").finish_non_exhaustive()
  }
}

impl MutationBarrierStorage for LocalMutationStorage {
  type Registration = LocalWakerRegistration;

  fn with_state<R>(&self, operation: impl FnOnce(&mut MutationBarrierState<Self::Registration>) -> R) -> R {
    let mut state = self.0.take();
    let output = operation(&mut state);
    self.0.set(state);
    output
  }
}

/// Thread-safe mutation storage for [`ConcurrentServer`].
#[derive(Clone, Debug, Default)]
struct ConcurrentMutationStorage(Arc<Mutex<MutationBarrierState<ConcurrentWakerRegistration>>>);

impl MutationBarrierStorage for ConcurrentMutationStorage {
  type Registration = ConcurrentWakerRegistration;

  fn with_state<R>(&self, operation: impl FnOnce(&mut MutationBarrierState<Self::Registration>) -> R) -> R {
    operation(&mut self.0.lock())
  }
}

/// Shared ordered-mutation issue and completion behavior.
#[derive(Clone, Debug, Default)]
struct MutationBarrier<S: MutationBarrierStorage> {
  /// Synchronously issued and committed revisions plus waiting tasks.
  state: S,
}

impl<S: MutationBarrierStorage> MutationBarrier<S> {
  /// Issue the next ordered mutation revision.
  fn issue(&self) -> Result<u64, ServerError> {
    self.state.with_state(|state| {
      state.issued = state.issued.checked_add(1).ok_or(ServerError::MutationSequenceExhausted)?;
      Ok(state.issued)
    })
  }

  /// Capture the newest mutation revision issued before a request.
  fn capture(&self) -> u64 {
    self.state.with_state(|state| state.issued)
  }

  /// Wait until the supplied mutation revision is committed.
  fn wait_for(&self, revision: u64) -> MutationBarrierWait<S> {
    MutationBarrierWait {
      barrier: self.clone(),
      revision,
      registration: S::Registration::default(),
    }
  }

  /// Commit one ordered mutation and wake every newly satisfied waiter.
  fn commit(&self, revision: u64) {
    let wakers = self.state.with_state(|state| {
      if revision <= state.committed || !state.completed.insert(revision) {
        return Vec::new();
      }
      state.committed = successors(state.committed.checked_add(1), |current| current.checked_add(1))
        .take_while(|candidate| state.completed.remove(candidate))
        .last()
        .unwrap_or(state.committed);
      let (ready, remaining): (Vec<_>, Vec<_>) = mem::take(&mut state.waiters)
        .into_iter()
        .partition(|waiter| waiter.revision <= state.committed);
      state.waiters = remaining;
      ready
        .into_iter()
        .filter_map(|waiter| waiter.registration.take())
        .collect::<Vec<_>>()
    });
    for waker in wakers {
      waker.wake();
    }
  }
}

/// Mutable state protected by [`MutationBarrier`].
#[derive(Debug, Default)]
struct MutationBarrierState<R: WakerRegistration> {
  /// Last issued mutation revision.
  issued:    u64,
  /// Last committed mutation revision.
  committed: u64,
  /// Completed revisions waiting for every earlier mutation to finish.
  completed: HashSet<u64>,
  /// Tasks waiting for a committed revision.
  waiters:   Vec<MutationWaiter<R>>,
}

/// Issued mutation whose drop marks completion even if its handler future is cancelled.
#[derive(Debug)]
struct MutationTicket<S: MutationBarrierStorage> {
  /// Shared ordering barrier.
  barrier:  MutationBarrier<S>,
  /// Issued mutation revision.
  revision: u64,
}

impl<S: MutationBarrierStorage> MutationTicket<S> {
  /// Wait for every earlier mutation to complete.
  fn wait_turn(&self) -> MutationBarrierWait<S> {
    self.barrier.wait_for(self.revision.saturating_sub(1))
  }
}

impl<S: MutationBarrierStorage> Drop for MutationTicket<S> {
  fn drop(&mut self) {
    self.barrier.commit(self.revision);
  }
}

/// One task waiting for an ordered mutation revision.
#[derive(Debug)]
struct MutationWaiter<R: WakerRegistration> {
  /// Required committed revision.
  revision:     u64,
  /// Stable registration updated by exactly one waiting future.
  registration: R,
}

/// Future resolving once a mutation revision is committed.
struct MutationBarrierWait<S: MutationBarrierStorage> {
  /// Shared mutation barrier.
  barrier:      MutationBarrier<S>,
  /// Required committed revision.
  revision:     u64,
  /// Stable registration used to replace this future's current waker.
  registration: S::Registration,
}

impl<S: MutationBarrierStorage> Future for MutationBarrierWait<S> {
  type Output = ();

  fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
    let this = self.as_ref().get_ref();
    this.barrier.state.with_state(|state| {
      if state.committed >= this.revision {
        return Poll::Ready(());
      }
      this.registration.replace(context.waker());
      if !state
        .waiters
        .iter()
        .any(|waiter| waiter.registration.is_same(&this.registration))
      {
        state.waiters.push(MutationWaiter {
          revision:     this.revision,
          registration: this.registration.clone(),
        });
      }
      Poll::Pending
    })
  }
}

impl<S: MutationBarrierStorage> Drop for MutationBarrierWait<S> {
  fn drop(&mut self) {
    self.barrier.state.with_state(|state| {
      state.waiters.retain(|waiter| !waiter.registration.is_same(&self.registration));
    });
    self.registration.clear();
  }
}

/// Ordering work captured synchronously when a message enters a server.
#[derive(Debug)]
enum MessageSchedule<S: MutationBarrierStorage> {
  /// Await every mutation issued before this request.
  Request {
    /// Last prior mutation revision.
    prior_revision: u64,
  },
  /// Await and then commit one ordered mutation.
  Mutation {
    /// This mutation's revision.
    ticket: Result<MutationTicket<S>, ServerError>,
  },
  /// No mutation ordering applies.
  Independent,
}

/// Capture request/mutation ordering at the synchronous message-entry boundary.
fn schedule_message<S: MutationBarrierStorage>(
  message: &rpc::Message,
  mutation_methods: &HashSet<&'static str>,
  mutation_barrier: &MutationBarrier<S>,
) -> MessageSchedule<S> {
  if message.method.is_some() && message.id.is_value() {
    MessageSchedule::Request {
      prior_revision: mutation_barrier.capture(),
    }
  } else if message.id.is_missing()
    && message
      .method
      .as_deref()
      .is_some_and(|method| mutation_methods.contains(method))
  {
    MessageSchedule::Mutation {
      ticket: mutation_barrier.issue().map(|revision| MutationTicket {
        barrier: mutation_barrier.clone(),
        revision,
      }),
    }
  } else {
    MessageSchedule::Independent
  }
}

/// Current-thread future that resolves when its inbound request is cancelled.
#[derive(Debug, Clone)]
pub struct LocalCancelToken {
  /// Shared local cancellation flag.
  cancelled: Rc<Cell<bool>>,
  /// Independently owned waker registration.
  waiter:    CancellationWaiter<LocalCancellationRegistry>,
}

impl LocalCancelToken {
  /// Return whether cancellation has been requested.
  #[must_use]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled.get()
  }

  /// Adapt cancellation into an RPC request-cancelled error.
  pub const fn as_error(&mut self) -> LocalCancelTokenError<'_> {
    LocalCancelTokenError(self)
  }
}

/// Implement the shared future contracts for one cancellation-token family.
macro_rules! implement_cancel_token_futures {
  (
    token = $token:ident,
    error = $error:ident,
    $(#[$error_attribute:meta])*
  ) => {
    impl Future for $token {
      type Output = ();

      fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.as_ref().get_ref();
        poll_cancellation(&this.waiter, context, || this.is_cancelled())
      }
    }

    impl FusedFuture for $token {
      fn is_terminated(&self) -> bool {
        self.is_cancelled()
      }
    }

    $(#[$error_attribute])*
    #[derive(Debug)]
    pub struct $error<'token>(&'token mut $token);

    impl Future for $error<'_> {
      type Output = Result<(), rpc::RpcError>;

      fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        poll_cancellation_error(self.get_mut().0, context)
      }
    }

    impl FusedFuture for $error<'_> {
      fn is_terminated(&self) -> bool {
        self.0.is_terminated()
      }
    }
  };
}

implement_cancel_token_futures!(
  token = LocalCancelToken,
  error = LocalCancelTokenError,
  /// Local cancellation future adapted to the LSP request-cancelled error.
);

/// Thread-safe future that resolves when its inbound request is cancelled.
#[derive(Debug, Clone)]
pub struct ConcurrentCancelToken {
  /// Shared concurrent cancellation flag.
  cancelled: Arc<AtomicBool>,
  /// Independently owned waker registration.
  waiter:    CancellationWaiter<ConcurrentCancellationRegistry>,
}

impl ConcurrentCancelToken {
  /// Return whether cancellation has been requested.
  #[must_use]
  pub fn is_cancelled(&self) -> bool {
    self.cancelled.load(Ordering::SeqCst)
  }

  /// Adapt cancellation into an RPC request-cancelled error.
  pub const fn as_error(&mut self) -> ConcurrentCancelTokenError<'_> {
    ConcurrentCancelTokenError(self)
  }
}

implement_cancel_token_futures!(
  token = ConcurrentCancelToken,
  error = ConcurrentCancelTokenError,
  /// Concurrent cancellation future adapted to the LSP request-cancelled error.
);

/// Protocol lifecycle state for one JSON-RPC session.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleState {
  /// No successful initialization has completed.
  Uninitialized,
  /// An initialization request is currently active.
  Initializing,
  /// Initialization completed and ordinary work is accepted.
  Initialized,
  /// A successful shutdown response has been delivered.
  ShuttingDown,
}

/// Mutable state shared by one server family for a single JSON-RPC session.
struct SessionState<C: CancellationState> {
  /// Next numeric ID for an outbound request.
  next_request_id:  i32,
  /// Current protocol lifecycle phase.
  lifecycle:        LifecycleState,
  /// Active inbound request cancellation states.
  active_tasks:     HashMap<rpc::RequestId, C>,
  /// Pending outbound requests awaiting client responses.
  pending_requests: HashMap<rpc::RequestId, oneshot::Sender<rpc::Response<serde_json::Value>>>,
}

impl<C: CancellationState> SessionState<C> {
  /// Construct a fresh protocol session.
  fn new() -> Self {
    Self {
      next_request_id:  0,
      lifecycle:        LifecycleState::Uninitialized,
      active_tasks:     HashMap::new(),
      pending_requests: HashMap::new(),
    }
  }

  /// Allocate a unique numeric outbound request ID.
  fn next_outbound_id(&mut self) -> Result<rpc::RequestId, ServerError> {
    let request_id = self.next_request_id;
    self.next_request_id = request_id.checked_add(1).ok_or(ServerError::RequestIdExhausted)?;
    Ok(NumberOrString::Number(request_id))
  }

  /// Begin one inbound request after validating lifecycle and duplicate-ID rules.
  fn begin_request(&mut self, request: &InboundRequest) -> Result<(RequestKind, C::Token), rpc::RpcError> {
    let kind = RequestKind::from_method(&request.method);
    if let Some(error) = lifecycle_error(self, kind) {
      return Err(error);
    }
    let token = match self.active_tasks.entry(request.id.clone()) {
      Entry::Occupied(_) => {
        return Err(rpc::RpcError::invalid_request().with_details("request ID is already active"));
      }
      Entry::Vacant(entry) => entry.insert(C::default()).token(),
    };
    if kind == RequestKind::Initialize {
      self.lifecycle = LifecycleState::Initializing;
    }
    Ok((kind, token))
  }

  /// Commit successful lifecycle transitions after a response reaches the writer.
  const fn commit_request(&mut self, kind: RequestKind, succeeded: bool) {
    match kind {
      RequestKind::Initialize => {
        self.lifecycle = if succeeded {
          LifecycleState::Initialized
        } else {
          LifecycleState::Uninitialized
        };
      }
      RequestKind::Shutdown if succeeded => {
        self.lifecycle = LifecycleState::ShuttingDown;
      }
      RequestKind::Shutdown | RequestKind::Ordinary => {}
    }
  }

  /// Remove a completed inbound request.
  fn finish_request(&mut self, id: &rpc::RequestId) {
    let was_active = self.active_tasks.remove(id).is_some();
    tracing::trace!(?id, was_active, "task completed");
  }

  /// Abort a request whose response could not be written.
  fn abort_request(&mut self, id: &rpc::RequestId, kind: RequestKind) {
    if kind == RequestKind::Initialize {
      self.lifecycle = LifecycleState::Uninitialized;
    }
    let was_active = self.active_tasks.remove(id).is_some();
    tracing::trace!(?id, was_active, "task aborted before response delivery");
  }

  /// Remove and cancel one active task.
  fn cancel_task(&self, id: &rpc::RequestId) {
    if let Some(cancellation) = self.active_tasks.get(id) {
      cancellation.cancel();
      tracing::trace!(?id, "task cancelled");
    }
  }

  /// Return whether initialization completed successfully.
  const fn is_initialized(&self) -> bool {
    matches!(self.lifecycle, LifecycleState::Initialized | LifecycleState::ShuttingDown)
  }

  /// Return whether a successful shutdown response has been sent.
  const fn is_shutting_down(&self) -> bool {
    matches!(self.lifecycle, LifecycleState::ShuttingDown)
  }

  /// Return whether ordinary notifications may mutate initialized session state.
  const fn accepts_notifications(&self) -> bool {
    matches!(self.lifecycle, LifecycleState::Initialized)
  }

  /// Apply shared notification lifecycle and cancellation transitions.
  fn prepare_notification(&self, notification: &InboundNotification) -> Result<NotificationDisposition, ServerError> {
    if notification.jsonrpc != "2.0" {
      tracing::warn!(method = ?notification.method, "ignoring notification with unsupported JSON-RPC version");
      return Ok(NotificationDisposition::Handled);
    }
    if notification.method == notification_types::Cancel::METHOD {
      if let Some(params) = notification.params.clone() {
        match serde_json::from_value::<lsp_types::CancelParams>(params) {
          Ok(cancel) => self.cancel_task(&cancel.id),
          Err(error) => tracing::warn!(?error, "invalid cancellation parameters"),
        }
      }
      return Ok(NotificationDisposition::Handled);
    }
    if notification.method == notification_types::Exit::METHOD {
      return if self.is_shutting_down() {
        Ok(NotificationDisposition::Handled)
      } else {
        Err(ServerError::ExitBeforeShutdown)
      };
    }
    if !self.accepts_notifications() {
      tracing::warn!(
        method = ?notification.method,
        "ignoring notification outside the initialized session"
      );
      return Ok(NotificationDisposition::Handled);
    }
    Ok(NotificationDisposition::Dispatch)
  }
}

/// Result of applying notification lifecycle transitions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NotificationDisposition {
  /// The shared session transition fully handled or ignored the notification.
  Handled,
  /// A registered runtime-specific handler may receive the notification.
  Dispatch,
}

/// The lifecycle significance of an inbound request method.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestKind {
  /// The standard initialize request.
  Initialize,
  /// The standard shutdown request.
  Shutdown,
  /// Any ordinary request.
  Ordinary,
}

impl RequestKind {
  /// Classify a request method for lifecycle transitions.
  #[allow(
    clippy::single_call_fn,
    reason = "method classification isolates initialize and shutdown lifecycle categories from session dispatch"
  )]
  fn from_method(method: &str) -> Self {
    if method == request_types::Initialize::METHOD {
      Self::Initialize
    } else if method == request_types::Shutdown::METHOD {
      Self::Shutdown
    } else {
      Self::Ordinary
    }
  }
}

/// Return any lifecycle error that must reject a request before handler invocation.
#[allow(
  clippy::single_call_fn,
  reason = "lifecycle validation centralizes pre-initialization and shutdown rejection before handler lookup"
)]
fn lifecycle_error<C: CancellationState>(state: &SessionState<C>, kind: RequestKind) -> Option<rpc::RpcError> {
  if state.lifecycle == LifecycleState::ShuttingDown {
    return Some(rpc::RpcError::invalid_request().with_details("server is shutting down"));
  }
  if kind == RequestKind::Initialize && state.lifecycle != LifecycleState::Uninitialized {
    return Some(rpc::RpcError::invalid_request().with_details("server is already initialized or initializing"));
  }
  if state.lifecycle != LifecycleState::Initialized && kind != RequestKind::Initialize {
    return Some(rpc::RpcError::server_not_initialized());
  }
  None
}

/// A classified inbound JSON-RPC message.
#[derive(Debug)]
enum Inbound {
  /// A client request with a concrete ID.
  Request(InboundRequest),
  /// A client notification without an ID.
  Notification(InboundNotification),
  /// A client response to an outbound server request.
  Response(rpc::Response<serde_json::Value>),
  /// A malformed request that requires a JSON-RPC error response.
  InvalidRequest(rpc::Message),
  /// A malformed client response that cannot itself receive a response.
  InvalidResponse(ServerError),
}

/// Concrete request data established at wire classification.
#[derive(Debug)]
struct InboundRequest {
  /// JSON-RPC version.
  jsonrpc: String,
  /// Method identifier.
  method:  String,
  /// Concrete request ID.
  id:      rpc::RequestId,
  /// Raw optional parameters.
  params:  Option<serde_json::Value>,
}

/// Concrete notification data established at wire classification.
#[derive(Debug)]
struct InboundNotification {
  /// JSON-RPC version.
  jsonrpc: String,
  /// Method identifier.
  method:  String,
  /// Raw optional parameters.
  params:  Option<serde_json::Value>,
}

/// Classify a wire message without consulting either runtime's mutable state.
fn classify_message(message: rpc::Message) -> Inbound {
  let recovered_id = match message.id.clone() {
    rpc::MessageId::Value(request_id) => rpc::MessageId::Value(request_id),
    rpc::MessageId::Missing | rpc::MessageId::Null => rpc::MessageId::Null,
  };
  if message.method.is_some()
    && message
      .params
      .as_ref()
      .is_some_and(|params| !params.is_array() && !params.is_object())
  {
    return Inbound::InvalidRequest(protocol_error_message(
      recovered_id,
      rpc::RpcError::invalid_request().with_details("params must be an object or array"),
    ));
  }
  if message.method.is_some() && (message.result.is_some() || message.error.is_some()) {
    return Inbound::InvalidRequest(protocol_error_message(
      recovered_id,
      rpc::RpcError::invalid_request().with_details("request messages cannot contain result or error fields"),
    ));
  }

  match (message.method, message.id) {
    (Some(method), rpc::MessageId::Value(id)) => Inbound::Request(InboundRequest {
      jsonrpc: message.jsonrpc,
      method,
      id,
      params: message.params,
    }),
    (Some(method), rpc::MessageId::Missing) => Inbound::Notification(InboundNotification {
      jsonrpc: message.jsonrpc,
      method,
      params: message.params,
    }),
    (Some(_), rpc::MessageId::Null) | (None, rpc::MessageId::Missing | rpc::MessageId::Null) => {
      Inbound::InvalidRequest(protocol_error_message(
        rpc::MessageId::Null,
        rpc::RpcError::invalid_request().with_details("a request method requires a concrete string or integer ID"),
      ))
    }
    (None, rpc::MessageId::Value(id)) if message.params.is_some() => Inbound::InvalidResponse(ServerError::InvalidResponseShape {
      id,
    }),
    (None, rpc::MessageId::Value(id)) => Inbound::Response(rpc::Response {
      jsonrpc: message.jsonrpc,
      id,
      result: message.result,
      error: message.error,
    }),
  }
}

/// Serialize optional typed parameters into JSON.
fn serialize_optional<T: Serialize>(input: Option<T>, context: &'static str) -> Result<Option<serde_json::Value>, ServerError> {
  input
    .map(serde_json::to_value)
    .transpose()
    .map_err(|source| ServerError::Serialization {
      context,
      source,
    })
}

/// Build a request-or-notification wire message from already serialized parameters.
fn request_message(method: &str, id: Option<rpc::RequestId>, params: Option<serde_json::Value>) -> rpc::Message {
  rpc::Message {
    jsonrpc: "2.0".into(),
    method: Some(method.into()),
    id: rpc::MessageId::from_optional(id),
    params,
    result: None,
    error: None,
  }
}

/// Build an error response for one concrete request ID.
fn error_message(id: rpc::RequestId, error: rpc::RpcError) -> rpc::Message {
  protocol_error_message(rpc::MessageId::Value(id), error)
}

/// Build an error response using a concrete or JSON `null` wire identifier.
fn protocol_error_message(id: rpc::MessageId, error: rpc::RpcError) -> rpc::Message {
  rpc::Message {
    jsonrpc: "2.0".into(),
    method: None,
    id,
    params: None,
    result: None,
    error: Some(error),
  }
}

/// Convert an erased handler outcome into a same-ID response message.
fn outcome_message(id: rpc::RequestId, outcome: RequestOutcome) -> rpc::Message {
  match outcome {
    RequestOutcome::Success(result) => rpc::Message {
      jsonrpc: "2.0".into(),
      method:  None,
      id:      rpc::MessageId::Value(id),
      params:  None,
      result:  Some(result),
      error:   None,
    },
    RequestOutcome::RpcError(error) => error_message(id, error),
    RequestOutcome::InvalidParams(detail) => error_message(id, rpc::RpcError::invalid_params().with_details(detail)),
    RequestOutcome::SerializationFailure(detail) => error_message(id, rpc::RpcError::internal_error().with_details(detail)),
  }
}

/// Wrapper around optional typed handler parameters.
#[derive(Debug)]
pub struct Params<P>(Option<P>);

impl<P> Params<P> {
  /// Return optional parameters unchanged.
  #[must_use]
  pub fn optional(self) -> Option<P> {
    self.0
  }

  /// Require parameters or return the standard invalid-params error.
  ///
  /// # Errors
  ///
  /// Returns an invalid-parameters RPC error when no parameters were supplied.
  pub fn required(self) -> Result<P, rpc::RpcError> {
    self
      .0
      .ok_or_else(|| rpc::RpcError::invalid_params().with_details("params are required"))
  }
}

impl<P> From<Option<P>> for Params<P> {
  fn from(params: Option<P>) -> Self {
    Self(params)
  }
}

#[cfg(test)]
mod tests {
  use std::cell::Cell;
  use std::collections::HashSet;
  use std::future::Future;
  use std::io;
  use std::pin::Pin;
  use std::rc::Rc;
  use std::sync::Arc;
  use std::sync::atomic::AtomicBool;
  use std::sync::atomic::AtomicU64;
  use std::sync::atomic::Ordering;
  use std::sync::mpsc;
  use std::task::Context;
  use std::task::Poll;
  use std::task::Wake;
  use std::task::Waker;
  use std::thread;
  use std::thread::Builder;
  use std::time::Duration;

  use futures::FutureExt as _;
  use futures::Sink;
  use futures::executor::block_on;
  use futures::future::BoxFuture;
  use futures::future::FusedFuture as _;
  use futures::future::LocalBoxFuture;
  use futures::future::Pending;
  use futures::future::Ready;
  use futures::future::join;
  use futures::future::join3;
  use futures::future::pending;
  use futures::future::poll_fn;
  use futures::future::ready;
  use futures::task::noop_waker;
  use lsp_types::NumberOrString;
  use lsp_types::notification;
  use lsp_types::notification::Notification;
  use lsp_types::request;
  use lsp_types::request::Request;
  use strict_test_support::ComparisonFailure;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_eq;
  use strict_test_support::ensure_that;

  use super::CancellationState;
  use super::CancellationWaiter;
  use super::ConcurrentCancelToken;
  use super::ConcurrentCancellation;
  use super::ConcurrentContext;
  use super::ConcurrentMutationStorage;
  use super::ConcurrentServer;
  use super::Inbound;
  use super::InboundNotification;
  use super::InboundRequest;
  use super::LocalCancellation;
  use super::LocalCancellationRegistry;
  use super::LocalContext;
  use super::LocalMutationStorage;
  use super::LocalServer;
  use super::MessageSchedule;
  use super::MessageWriterError;
  use super::MutationBarrier;
  use super::MutationBarrierStorage;
  use super::MutationTicket;
  use super::NotificationDisposition;
  use super::Params;
  use super::RequestKind;
  use super::RequestOutcome;
  use super::SerializationFailureFixture;
  use super::ServerError;
  use super::SessionState;
  use super::classify_message;
  use super::outcome_message;
  use super::poll_cancellation;
  use super::rpc;
  use super::schedule_message;
  use super::serialize_optional;

  /// Count wakeups delivered to one independently registered cancellation waiter.
  #[derive(Debug, Default)]
  struct WakeCounter {
    /// Number of wakeups observed by this waiter.
    count: AtomicU64,
  }

  impl WakeCounter {
    /// Record one wake without wrapping the observation counter.
    fn record_wake(&self) {
      let reached_maximum = self
        .count
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_add(1))
        .is_err();
      if reached_maximum {
        self.count.store(u64::MAX, Ordering::SeqCst);
      }
    }
  }

  impl Wake for WakeCounter {
    fn wake(self: Arc<Self>) {
      self.record_wake();
    }

    fn wake_by_ref(self: &Arc<Self>) {
      self.record_wake();
    }
  }

  /// Two independently observable task wakers used by waiter lifecycle tests.
  #[derive(Debug)]
  struct WakeObservers {
    /// Wake count retained for the first task.
    first_counter:  Arc<WakeCounter>,
    /// Wake count retained for the second task.
    second_counter: Arc<WakeCounter>,
    /// Waker registered by the first task.
    first_waker:    Waker,
    /// Waker registered by the second task.
    second_waker:   Waker,
  }

  impl WakeObservers {
    /// Construct two independent counters and their corresponding wakers.
    fn new() -> Self {
      let first_counter = Arc::new(WakeCounter::default());
      let second_counter = Arc::new(WakeCounter::default());
      let first_waker = Waker::from(Arc::clone(&first_counter));
      let second_waker = Waker::from(Arc::clone(&second_counter));
      Self {
        first_counter,
        second_counter,
        first_waker,
        second_waker,
      }
    }
  }

  /// Thread-safe in-memory message sink shared by local and concurrent runtime tests.
  #[derive(Clone, Debug, Default)]
  struct TestWriter {
    /// Messages accepted by the sink.
    messages: Arc<parking_lot::Mutex<Vec<rpc::Message>>>,
    /// Whether the next and subsequent writes should fail.
    fail:     Arc<AtomicBool>,
  }

  impl TestWriter {
    /// Construct a writer that rejects every message.
    #[allow(
      clippy::single_call_fn,
      reason = "the failing-writer fixture explicitly constructs the outbound transport-error path under test"
    )]
    fn failing() -> Self {
      Self {
        messages: Arc::default(),
        fail:     Arc::new(AtomicBool::new(true)),
      }
    }

    /// Clone every accepted message.
    fn messages(&self) -> Vec<rpc::Message> {
      self.messages.lock().clone()
    }

    /// Clone the result emitted for one protocol request identifier.
    fn result_for(&self, request_id: &rpc::MessageId) -> Option<serde_json::Value> {
      self
        .messages
        .lock()
        .iter()
        .find(|message| &message.id == request_id)
        .and_then(|message| message.result.clone())
    }

    /// Clone one parameter emitted by the first matching notification.
    fn notification_parameter(&self, method: &str, name: &str) -> Option<serde_json::Value> {
      self
        .messages
        .lock()
        .iter()
        .find(|message| message.method.as_deref() == Some(method))
        .and_then(|message| message.params.as_ref())
        .and_then(|params| params.get(name))
        .cloned()
    }

    /// Count messages emitted for one protocol method.
    fn method_count(&self, method: &str) -> usize {
      self
        .messages
        .lock()
        .iter()
        .filter(|message| message.method.as_deref() == Some(method))
        .count()
    }
  }

  impl Sink<rpc::Message> for TestWriter {
    type Error = MessageWriterError;

    fn poll_ready(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
      if self.fail.load(Ordering::SeqCst) {
        Poll::Ready(Err(MessageWriterError::from(io::Error::from(io::ErrorKind::BrokenPipe))))
      } else {
        Poll::Ready(Ok(()))
      }
    }

    fn start_send(self: Pin<&mut Self>, message: rpc::Message) -> Result<(), Self::Error> {
      if self.fail.load(Ordering::SeqCst) {
        return Err(MessageWriterError::from(io::Error::from(io::ErrorKind::BrokenPipe)));
      }
      self.messages.lock().push(message);
      Ok(())
    }

    crate::implement_message_writer_readiness!();
  }

  /// Test-only initialize request retaining the standard lifecycle method.
  enum InitializeRequest {}

  impl Request for InitializeRequest {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "initialize";
  }

  /// Mutation notification used to prove request-barrier visibility.
  enum SetValue {}

  impl Notification for SetValue {
    type Params = SetValueParams;
    const METHOD: &'static str = "fixture/setValue";
  }

  /// Value carried by [`SetValue`].
  #[derive(serde::Deserialize, serde::Serialize)]
  struct SetValueParams {
    /// Replacement value.
    #[serde(rename = "value")]
    replacement: u64,
  }

  /// Request used to read the state changed by [`SetValue`].
  enum GetValue {}

  impl Request for GetValue {
    type Params = ();
    type Result = u64;
    const METHOD: &'static str = "fixture/getValue";
  }

  /// Request whose handlers block until the overlap test releases them.
  enum OverlapRequest {}

  impl Request for OverlapRequest {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "fixture/overlap";
  }

  /// Request that remains pending until the server cancels it.
  enum PendingRequest {}

  impl Request for PendingRequest {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "fixture/pending";
  }

  /// Outbound request used to exercise response delivery and cleanup.
  enum OutboundRequest {}

  impl Request for OutboundRequest {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "fixture/outbound";
  }

  /// Inbound request whose handler waits for one outbound response.
  enum OutboundProbe {}

  impl Request for OutboundProbe {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "fixture/outboundProbe";
  }

  /// Request that exercises the complete public handler context.
  enum ContextProbe {}

  impl Request for ContextProbe {
    type Params = ();
    type Result = u64;
    const METHOD: &'static str = "fixture/contextProbe";
  }

  /// Notification emitted by [`ContextProbe`] through the public writer API.
  enum ProbeNotification {}

  impl Notification for ProbeNotification {
    type Params = ProbeNotificationParams;
    const METHOD: &'static str = "fixture/probeNotification";
  }

  /// Context observation carried by [`ProbeNotification`].
  #[derive(Debug, serde::Deserialize, serde::Serialize)]
  struct ProbeNotificationParams {
    /// Value observed before deferred work runs.
    observed: u64,
  }

  /// Outbound request carrying a scalar response for typed routing tests.
  enum OutboundValueRequest {}

  impl Request for OutboundValueRequest {
    type Params = ();
    type Result = u64;
    const METHOD: &'static str = "fixture/outboundValue";
  }

  /// Inbound request whose result mirrors one outbound scalar response.
  enum OutboundValueProbe {}

  impl Request for OutboundValueProbe {
    type Params = ();
    type Result = u64;
    const METHOD: &'static str = "fixture/outboundValueProbe";
  }

  /// Inbound request that issues and then explicitly cancels an outbound request.
  enum OutboundCancelProbe {}

  impl Request for OutboundCancelProbe {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "fixture/outboundCancelProbe";
  }

  /// Yield one poll so a barrier test can observe interleaving.
  fn yield_once() -> impl Future<Output = ()> {
    let mut first_poll = true;
    poll_fn(move |context| {
      if first_poll {
        first_poll = false;
        context.waker().wake_by_ref();
        Poll::Pending
      } else {
        Poll::Ready(())
      }
    })
  }

  /// Construct one request or notification test message.
  fn message(method: &str, id: rpc::MessageId, params: Option<serde_json::Value>) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(method.into()),
      id,
      params,
      result: None,
      error: None,
    }
  }

  /// Construct one client response for an outbound fixture request.
  fn response_message(jsonrpc: &str, id: i32, result: Option<serde_json::Value>, error: Option<rpc::RpcError>) -> rpc::Message {
    rpc::Message {
      jsonrpc: jsonrpc.into(),
      method: None,
      id: rpc::MessageId::Value(NumberOrString::Number(id)),
      params: None,
      result,
      error,
    }
  }

  /// Construct one concrete-ID inbound request for lifecycle transition tests.
  fn inbound_request(method: &str, id: i32) -> InboundRequest {
    InboundRequest {
      jsonrpc: "2.0".into(),
      method:  method.into(),
      id:      NumberOrString::Number(id),
      params:  None,
    }
  }

  /// Collect sorted JSON-RPC error codes emitted for one concrete request identifier.
  fn response_error_codes(writer: &TestWriter, request_id: NumberOrString) -> Vec<i32> {
    let wire_id = rpc::MessageId::Value(request_id);
    let mut codes = writer
      .messages()
      .into_iter()
      .filter(|response| response.id == wire_id)
      .filter_map(|response| response.error.map(|error| error.code))
      .collect::<Vec<_>>();
    codes.sort_unstable();
    codes
  }

  /// Native completion and context for one public server operation.
  type DispatchObservation = (&'static str, Result<(), ServerError>);

  /// Drive both halves of one exchange and retain both native completions.
  fn complete_exchange<InboundFuture, ResponseFuture>(
    inbound: InboundFuture,
    response: ResponseFuture,
    inbound_context: &'static str,
    response_context: &'static str,
  ) -> [DispatchObservation; 2]
  where
    InboundFuture: Future<Output = Result<(), ServerError>>,
    ResponseFuture: Future<Output = Result<(), ServerError>>,
  {
    let (inbound_result, response_result) = block_on(join(inbound, response));
    [(inbound_context, inbound_result), (response_context, response_result)]
  }

  /// Complete runtime fixture and every observed operation or state checkpoint.
  #[derive(Debug)]
  struct RuntimeObservations<W, S> {
    /// World owner retained through assertion completion.
    world:    W,
    /// Writer retaining every emitted message and its transport state.
    writer:   TestWriter,
    /// Runtime owner retaining protocol state and registries.
    server:   S,
    /// Native results in dispatch order with their scenario contexts.
    dispatch: Vec<DispatchObservation>,
    /// World values observed before later mutations.
    values:   Vec<u64>,
    /// Message snapshots observed before later dispatch.
    messages: Vec<Vec<rpc::Message>>,
    /// Registry descriptions captured before and after handler replacement.
    registry: Vec<String>,
  }

  /// Native terminal assertion over a retained runtime fixture.
  type RuntimeAssertion<W, S> = Result<(), Box<PredicateFailure<RuntimeObservations<W, S>>>>;

  /// Current-thread state used by local runtime behavior tests.
  type LocalValue = Rc<Cell<u64>>;

  /// Complete initialization identically for either honest runtime context family.
  fn initialize_runtime_context<C>(_context: C, _params: Params<()>) -> Ready<Result<(), rpc::RpcError>> {
    ready(Ok(()))
  }

  /// Commit one local value after yielding once.
  fn set_local_value(
    context: LocalContext<LocalValue>,
    params: Params<SetValueParams>,
  ) -> LocalBoxFuture<'static, Result<(), ServerError>> {
    async move {
      yield_once().await;
      if let Some(parameters) = params.optional() {
        context.world().set(parameters.replacement);
      }
      Ok(())
    }
    .boxed_local()
  }

  /// Read one local value.
  fn get_local_value(context: LocalContext<LocalValue>, _params: Params<()>) -> LocalBoxFuture<'static, Result<u64, rpc::RpcError>> {
    async move { Ok(context.world().get()) }.boxed_local()
  }

  /// Complete the shared outbound-cleanup behavior after one response channel resolves.
  async fn finish_outbound_cleanup<T, C>(response: Result<T, ServerError>, cancellation: C) -> Result<(), rpc::RpcError>
  where
    C: Future<Output = Result<(), ServerError>>,
  {
    match response {
      Err(ServerError::ResponseChannelClosed) => cancellation
        .await
        .map_err(|error| rpc::RpcError::internal_error().with_details(error.to_string())),
      Ok(_response) => {
        Err(rpc::RpcError::internal_error().with_details("the malformed response unexpectedly completed the outbound request"))
      }
      Err(error) => Err(rpc::RpcError::internal_error().with_details(error.to_string())),
    }
  }

  /// Remain pending so local cancellation and duplicate-ID handling can race it.
  #[allow(
    clippy::single_call_fn,
    reason = "the local pending handler names the in-flight cancellation fixture registered by the runtime-family tests"
  )]
  fn pending_local(_context: LocalContext<LocalValue>, _params: Params<()>) -> Pending<Result<(), rpc::RpcError>> {
    pending()
  }

  /// Observe a closed local outbound-response channel and then attempt cancellation.
  #[allow(
    clippy::single_call_fn,
    reason = "the local outbound probe isolates pending-response cleanup behavior in the runtime-family fixture"
  )]
  fn probe_local_outbound_cleanup(
    mut context: LocalContext<LocalValue>,
    _params: Params<()>,
  ) -> LocalBoxFuture<'static, Result<(), rpc::RpcError>> {
    async move {
      let response = context.write_request::<OutboundRequest>(None).await;
      let cancellation = context.cancel();
      finish_outbound_cleanup(response, cancellation).await
    }
    .boxed_local()
  }

  /// Concurrent state used by ordering tests.
  #[derive(Clone, Debug, Default)]
  struct ConcurrentValue(Arc<AtomicU64>);

  /// Shared scalar behavior required by runtime-family handler fixtures.
  trait RuntimeValue {
    /// Load the currently observable value.
    fn load_value(&self) -> u64;

    /// Store one deferred replacement value.
    fn store_value(&self, value: u64);
  }

  impl RuntimeValue for LocalValue {
    fn load_value(&self) -> u64 {
      self.get()
    }

    fn store_value(&self, value: u64) {
      self.set(value);
    }
  }

  impl RuntimeValue for ConcurrentValue {
    fn load_value(&self) -> u64 {
      self.0.load(Ordering::SeqCst)
    }

    fn store_value(&self, value: u64) {
      self.0.store(value, Ordering::SeqCst);
    }
  }

  /// Adapt one server-runtime failure to the handler's JSON-RPC boundary.
  fn runtime_rpc_error(error: &ServerError) -> rpc::RpcError {
    rpc::RpcError::internal_error().with_details(error.to_string())
  }

  /// Generate context and outbound-request handlers for one runtime family.
  macro_rules! runtime_handler_family {
    (
      context =
      $context:ident,world =
      $world:ty,future =
      $future:ident,box_method =
      $box_method:ident,context_probe =
      $context_probe:ident,value_probe =
      $value_probe:ident,cancel_probe =
      $cancel_probe:ident,deferred_value =
      $deferred_value:literal,debug_name =
      $debug_name:literal,
    ) => {
      /// Exercise context state, world access, notifications, and deferred work.
      #[allow(
        clippy::single_call_fn,
        reason = "the generated context probe names one runtime family's complete handler-context contract"
      )]
      fn $context_probe(mut context: $context<$world>, _params: Params<()>) -> $future<'static, Result<u64, rpc::RpcError>> {
        async move {
          let initialized = context.is_initialized().await;
          let shutting_down = context.is_shutting_down().await;
          let initial = context.world().load_value();
          let dereferenced: &$world = std::ops::Deref::deref(&context);
          let dereferenced_value = dereferenced.load_value();
          let cancelled = context.cancel_token().is_cancelled();
          let debug = format!("{context:?}");
          let deferred_world = context.world().clone();
          context
            .defer(async move {
              deferred_world.store_value($deferred_value);
              Ok(())
            })
            .await;
          context
            .write_notification::<ProbeNotification>(Some(ProbeNotificationParams {
              observed: initial
            }))
            .await
            .map_err(|error| runtime_rpc_error(&error))?;
          if initialized && !shutting_down && !cancelled && initial == dereferenced_value && debug.contains($debug_name) {
            Ok(initial)
          } else {
            Err(rpc::RpcError::internal_error().with_details("the runtime context exposed inconsistent invocation state"))
          }
        }
        .$box_method()
      }

      /// Mirror one typed outbound scalar response through the inbound request.
      #[allow(
        clippy::single_call_fn,
        reason = "the generated value probe names one runtime family's typed outbound response-routing contract"
      )]
      fn $value_probe(mut context: $context<$world>, _params: Params<()>) -> $future<'static, Result<u64, rpc::RpcError>> {
        async move {
          let response = context
            .write_request::<OutboundValueRequest>(None)
            .await
            .map_err(|error| runtime_rpc_error(&error))?;
          if let Some(error) = response.error {
            return Err(error);
          }
          response
            .result
            .ok_or_else(|| rpc::RpcError::internal_error().with_details("the outbound value response omitted its typed result"))
        }
        .$box_method()
      }

      /// Issue one outbound request, observe it pending, then send cancellation.
      #[allow(
        clippy::single_call_fn,
        reason = "the generated cancel probe names one runtime family's outbound cancellation-target contract"
      )]
      fn $cancel_probe(mut context: $context<$world>, _params: Params<()>) -> $future<'static, Result<(), rpc::RpcError>> {
        async move {
          let mut outbound = Box::pin(context.write_request::<OutboundValueRequest>(None));
          poll_fn(|poll_context| match Future::poll(outbound.as_mut(), poll_context) {
            Poll::Pending => Poll::Ready(Ok(())),
            Poll::Ready(Ok(_response)) => Poll::Ready(Err(
              rpc::RpcError::internal_error().with_details("the outbound request completed before cancellation"),
            )),
            Poll::Ready(Err(error)) => Poll::Ready(Err(runtime_rpc_error(&error))),
          })
          .await?;
          drop(outbound);
          context.cancel().await.map_err(|error| runtime_rpc_error(&error))
        }
        .$box_method()
      }
    };
  }

  runtime_handler_family!(
    context = LocalContext,
    world = LocalValue,
    future = LocalBoxFuture,
    box_method = boxed_local,
    context_probe = probe_local_context,
    value_probe = probe_local_outbound_value,
    cancel_probe = probe_local_outbound_cancellation,
    deferred_value = 91,
    debug_name = "LocalContext",
  );

  runtime_handler_family!(
    context = ConcurrentContext,
    world = ConcurrentValue,
    future = BoxFuture,
    box_method = boxed,
    context_probe = probe_concurrent_context,
    value_probe = probe_concurrent_outbound_value,
    cancel_probe = probe_concurrent_outbound_cancellation,
    deferred_value = 97,
    debug_name = "ConcurrentContext",
  );

  /// Commit one concurrent value after yielding once.
  fn set_concurrent_value(
    context: ConcurrentContext<ConcurrentValue>,
    params: Params<SetValueParams>,
  ) -> BoxFuture<'static, Result<(), ServerError>> {
    async move {
      yield_once().await;
      if let Some(parameters) = params.optional() {
        context.world().0.store(parameters.replacement, Ordering::SeqCst);
      }
      Ok(())
    }
    .boxed()
  }

  /// Read one concurrent value.
  fn get_concurrent_value(
    context: ConcurrentContext<ConcurrentValue>,
    _params: Params<()>,
  ) -> BoxFuture<'static, Result<u64, rpc::RpcError>> {
    async move { Ok(context.world().0.load(Ordering::SeqCst)) }.boxed()
  }

  /// Remain pending so concurrent cancellation and duplicate-ID handling can race it.
  #[allow(
    clippy::single_call_fn,
    reason = "the concurrent pending handler names the in-flight cancellation fixture registered by the runtime-family tests"
  )]
  fn pending_concurrent(_context: ConcurrentContext<ConcurrentValue>, _params: Params<()>) -> Pending<Result<(), rpc::RpcError>> {
    pending()
  }

  /// Observe a closed concurrent outbound-response channel and then attempt cancellation.
  #[allow(
    clippy::single_call_fn,
    reason = "the concurrent outbound probe isolates pending-response cleanup behavior in the runtime-family fixture"
  )]
  fn probe_concurrent_outbound_cleanup(
    mut context: ConcurrentContext<ConcurrentValue>,
    _params: Params<()>,
  ) -> BoxFuture<'static, Result<(), rpc::RpcError>> {
    async move {
      let response = context.write_request::<OutboundRequest>(None).await;
      let cancellation = context.cancel();
      finish_outbound_cleanup(response, cancellation).await
    }
    .boxed()
  }

  /// Shared state used to prove two concurrent handlers overlap.
  #[derive(Clone)]
  struct OverlapWorld {
    /// Handler-entry observations.
    started: mpsc::Sender<()>,
    /// Test-controlled release gate.
    release: Arc<AtomicBool>,
  }

  /// Successful concurrent initialize handler for overlap state.
  #[allow(
    clippy::single_call_fn,
    reason = "the overlap initialize handler establishes lifecycle readiness before concurrent scheduling is exercised"
  )]
  fn initialize_overlap(_context: ConcurrentContext<OverlapWorld>, _params: Params<()>) -> Ready<Result<(), rpc::RpcError>> {
    ready(Ok(()))
  }

  /// Block one request thread after reporting handler entry.
  #[allow(
    clippy::single_call_fn,
    reason = "the overlap handler isolates the concurrent-entry signal and test-controlled release barrier"
  )]
  fn overlap_handler(context: ConcurrentContext<OverlapWorld>, _params: Params<()>) -> BoxFuture<'static, Result<(), rpc::RpcError>> {
    async move {
      context
        .world()
        .started
        .send(())
        .map_err(|error| rpc::RpcError::internal_error().with_details(error.to_string()))?;
      while !context.world().release.load(Ordering::SeqCst) {
        thread::yield_now();
      }
      Ok(())
    }
    .boxed()
  }

  /// Require one concurrent protocol type to be transferable and shareable.
  fn require_send_sync<T: Send + Sync>() {}

  /// One native request admission, retaining both lifecycle kind and cancellation token.
  type RequestAdmission<T> = Result<(RequestKind, T), rpc::RpcError>;

  /// Native request admissions, notifications, and lifecycle checkpoints.
  #[derive(Debug)]
  struct LifecycleObservations<T> {
    /// All admission results, retaining successful cancellation tokens and RPC failures.
    requests: Vec<RequestAdmission<T>>,
    /// Exit results before and after shutdown.
    exits:    [Result<NotificationDisposition, ServerError>; 2],
    /// Lifecycle state after failed initialization and failed shutdown.
    recovery: [super::LifecycleState; 2],
  }

  impl<T> LifecycleObservations<T> {
    /// Inspect the complete shared lifecycle contract by reference.
    fn preserves_transitions(&self) -> bool {
      let expected = [
        Err(rpc::RpcError::server_not_initialized()),
        Ok(RequestKind::Initialize),
        Ok(RequestKind::Initialize),
        Err(rpc::RpcError::invalid_request().with_details("server is already initialized or initializing")),
        Ok(RequestKind::Ordinary),
        Ok(RequestKind::Shutdown),
        Ok(RequestKind::Ordinary),
        Ok(RequestKind::Shutdown),
        Err(rpc::RpcError::invalid_request().with_details("server is shutting down")),
      ];
      self.requests.len() == expected.len()
        && self
          .requests
          .iter()
          .zip(expected)
          .all(|(outcome, kind)| outcome.as_ref().map(|admission| &admission.0) == kind.as_ref())
        && matches!(self.exits, [
          Err(ServerError::ExitBeforeShutdown),
          Ok(NotificationDisposition::Handled)
        ])
        && self.recovery == [super::LifecycleState::Uninitialized, super::LifecycleState::Initialized]
    }
  }

  /// Exercise lifecycle transitions while preserving every request admission outcome.
  fn lifecycle_transitions<C: CancellationState>() -> LifecycleObservations<C::Token> {
    let mut session = SessionState::<C>::new();
    let ordinary = inbound_request("fixture", 2);
    let before = session.begin_request(&ordinary);
    let exit = InboundNotification {
      jsonrpc: "2.0".into(),
      method:  "exit".into(),
      params:  None,
    };
    let early_exit = session.prepare_notification(&exit);
    let failed_initialize = complete_request(&mut session, &inbound_request("initialize", 0), false);
    let after_failed_initialize = session.lifecycle;
    let initialize = inbound_request("initialize", 1);
    let initializing = session.begin_request(&initialize);
    let duplicate = session.begin_request(&inbound_request("initialize", 4));
    if let Ok(&(kind, ref _token)) = initializing.as_ref() {
      session.commit_request(kind, true);
      session.finish_request(&initialize.id);
    }
    let ordinary_result = complete_request(&mut session, &ordinary, true);
    let failed_shutdown = complete_request(&mut session, &inbound_request("shutdown", 5), false);
    let after_failed_shutdown = session.lifecycle;
    let recovered = complete_request(&mut session, &ordinary, true);
    let shutdown = complete_request(&mut session, &inbound_request("shutdown", 3), true);
    let after = session.begin_request(&ordinary);
    LifecycleObservations {
      requests: vec![
        before, failed_initialize, initializing, duplicate, ordinary_result, failed_shutdown, recovered, shutdown, after,
      ],
      exits:    [early_exit, session.prepare_notification(&exit)],
      recovery: [after_failed_initialize, after_failed_shutdown],
    }
  }

  /// Complete one admitted request while retaining its original token and lifecycle kind.
  fn complete_request<C: CancellationState>(
    session: &mut SessionState<C>,
    request: &InboundRequest,
    succeeded: bool,
  ) -> Result<(RequestKind, C::Token), rpc::RpcError> {
    let result = session.begin_request(request);
    if let Ok(&(kind, ref _token)) = result.as_ref() {
      session.commit_request(kind, succeeded);
      session.finish_request(&request.id);
    }
    result
  }

  /// Observe all notification guards without discarding successful dispositions.
  fn notification_guards<C: CancellationState>() -> [Result<NotificationDisposition, ServerError>; 4] {
    let session = SessionState::<C>::new();
    [
      InboundNotification {
        jsonrpc: "1.0".into(),
        method:  "fixture/observe".into(),
        params:  None,
      },
      InboundNotification {
        jsonrpc: "2.0".into(),
        method:  "fixture/observe".into(),
        params:  None,
      },
      InboundNotification {
        jsonrpc: "2.0".into(),
        method:  notification::Cancel::METHOD.into(),
        params:  Some(serde_json::json!({"id": {"invalid": true}})),
      },
      InboundNotification {
        jsonrpc: "2.0".into(),
        method:  notification::Cancel::METHOD.into(),
        params:  None,
      },
    ]
    .map(|notification| session.prepare_notification(&notification))
  }

  /// Native revision issuance, polling, and storage snapshots for one barrier scenario.
  #[derive(Debug)]
  struct MutationObservations {
    /// Every attempted revision allocation.
    issued:        [Result<u64, ServerError>; 2],
    /// Complete poll outcomes in effect order.
    polls:         Vec<Poll<()>>,
    /// Waiter counts captured before or after lifecycle effects.
    waiter_counts: Vec<usize>,
    /// Both task wake counts at each checkpoint.
    wake_counts:   Vec<[u64; 2]>,
    /// Uncommitted revisions retained after the tested effects.
    completed:     HashSet<u64>,
  }

  /// Exercise out-of-order barrier completion through one ownership model.
  fn mutation_ordering<S: MutationBarrierStorage>() -> MutationObservations {
    let barrier = MutationBarrier::<S>::default();
    let issued = [barrier.issue(), barrier.issue()];
    let mut polls = Vec::new();
    if let [Ok(first_revision), Ok(second_revision)] = issued {
      let first = MutationTicket {
        barrier:  barrier.clone(),
        revision: first_revision,
      };
      let second = MutationTicket {
        barrier:  barrier.clone(),
        revision: second_revision,
      };
      let mut wait = Box::pin(barrier.wait_for(second_revision));
      let waker = noop_waker();
      let mut context = Context::from_waker(&waker);
      polls.push(Future::poll(wait.as_mut(), &mut context));
      drop(second);
      polls.push(Future::poll(wait.as_mut(), &mut context));
      drop(first);
      polls.push(Future::poll(wait.as_mut(), &mut context));
    }
    MutationObservations {
      issued,
      polls,
      waiter_counts: Vec::new(),
      wake_counts: Vec::new(),
      completed: barrier.state.with_state(|state| state.completed.clone()),
    }
  }

  /// Exercise waiter replacement, duplicate completion, and owned drop-time deregistration.
  fn mutation_waiter_lifecycle<S: MutationBarrierStorage>() -> MutationObservations {
    let barrier = MutationBarrier::<S>::default();
    let first = barrier.issue();
    let mut polls = Vec::new();
    let mut waiter_counts = Vec::new();
    let mut wake_counts = Vec::new();
    let observers = WakeObservers::new();
    let mut first_context = Context::from_waker(&observers.first_waker);
    let mut second_context = Context::from_waker(&observers.second_waker);
    if let Ok(revision) = first {
      let ticket = MutationTicket {
        barrier: barrier.clone(),
        revision,
      };
      let mut wait = Box::pin(barrier.wait_for(revision));
      polls.push(Future::poll(wait.as_mut(), &mut first_context));
      polls.push(Future::poll(wait.as_mut(), &mut second_context));
      waiter_counts.push(barrier.state.with_state(|state| state.waiters.len()));
      drop(ticket);
      wake_counts.push([
        observers.first_counter.count.load(Ordering::SeqCst),
        observers.second_counter.count.load(Ordering::SeqCst),
      ]);
      polls.push(Future::poll(wait.as_mut(), &mut second_context));
      barrier.commit(revision);
      wake_counts.push([
        observers.first_counter.count.load(Ordering::SeqCst),
        observers.second_counter.count.load(Ordering::SeqCst),
      ]);
      waiter_counts.push(barrier.state.with_state(|state| state.waiters.len()));
    }
    let abandoned = barrier.issue();
    if let Ok(revision) = abandoned {
      let ticket = MutationTicket {
        barrier: barrier.clone(),
        revision,
      };
      let mut wait = Box::pin(barrier.wait_for(revision));
      polls.push(Future::poll(wait.as_mut(), &mut first_context));
      drop(wait);
      waiter_counts.push(barrier.state.with_state(|state| state.waiters.len()));
      drop(ticket);
    }
    MutationObservations {
      issued: [first, abandoned],
      polls,
      waiter_counts,
      wake_counts,
      completed: barrier.state.with_state(|state| state.completed.clone()),
    }
  }

  /// Native token registration polls and wake counts after cancellation.
  type CancellationWakeObservations = ([Poll<()>; 2], [u64; 2]);

  /// Observe independent clones and the effect of optionally dropping one registered clone.
  fn cancellation_wake_observations<C>(drop_first: bool) -> CancellationWakeObservations
  where
    C: CancellationState,
    C::Token: Future<Output = ()> + Unpin,
  {
    let cancellation = C::default();
    let mut first_token = cancellation.token();
    let mut second = first_token.clone();
    let observers = WakeObservers::new();
    let polls = [
      Future::poll(Pin::new(&mut first_token), &mut Context::from_waker(&observers.first_waker)),
      Future::poll(Pin::new(&mut second), &mut Context::from_waker(&observers.second_waker)),
    ];
    let retained_first = if drop_first {
      drop(first_token);
      None
    } else {
      Some(first_token)
    };
    cancellation.cancel();
    let wake_counts = [
      observers.first_counter.count.load(Ordering::SeqCst),
      observers.second_counter.count.load(Ordering::SeqCst),
    ];
    drop(retained_first);
    (polls, wake_counts)
  }

  /// Retained admission results and cancellation polls for completed, unknown, and active IDs.
  #[derive(Debug)]
  struct CancellationLookup<T> {
    /// Initialization, completed request, and active request admissions.
    requests: [RequestAdmission<T>; 3],
    /// Polls before and after cancelling the active identifier.
    polls:    Vec<[Poll<()>; 2]>,
  }

  /// Exercise cancellation lookup without converting admission errors to absence.
  fn cancellation_ignores_inactive_ids<C>() -> CancellationLookup<C::Token>
  where
    C: CancellationState,
    C::Token: Future<Output = ()> + Unpin,
  {
    let mut session = SessionState::<C>::new();
    let initialized = complete_request(&mut session, &inbound_request(InitializeRequest::METHOD, 0), true);
    let completed = inbound_request(PendingRequest::METHOD, 1);
    let mut completed_result = session.begin_request(&completed);
    session.finish_request(&completed.id);
    let active = inbound_request(PendingRequest::METHOD, 2);
    let mut active_result = session.begin_request(&active);
    session.cancel_task(&completed.id);
    session.cancel_task(&NumberOrString::Number(99));
    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    let mut polls = Vec::new();
    if let (Ok(&mut (_completed_kind, ref mut completed_token)), Ok(&mut (_active_kind, ref mut active_token))) =
      (completed_result.as_mut(), active_result.as_mut())
    {
      polls.push([
        Future::poll(Pin::new(&mut *completed_token), &mut context),
        Future::poll(Pin::new(&mut *active_token), &mut context),
      ]);
      session.cancel_task(&active.id);
      polls.push([
        Future::poll(Pin::new(active_token), &mut context),
        Future::poll(Pin::new(completed_token), &mut context),
      ]);
    }
    session.finish_request(&active.id);
    CancellationLookup {
      requests: [initialized, completed_result, active_result],
      polls,
    }
  }

  /// Both families' native notification guard results.
  type NotificationGuards = [[Result<NotificationDisposition, ServerError>; 4]; 2];

  /// Poll and cancellation flag at the registration race boundary.
  type CancellationRace = (Poll<()>, bool);

  /// Native clone registration and wake results for both families.
  type CancellationWakeFamilies = [CancellationWakeObservations; 2];

  /// Original RPC adapter results for both token families.
  type CancellationErrors = [Result<(), rpc::RpcError>; 2];

  /// Complete barrier scenarios for both storage families.
  type MutationFamilies = [MutationObservations; 2];

  /// Complete malformed message classifications.
  type InvalidClassifications = [Inbound; 6];

  /// Native serialization failure and its complete protocol response.
  type SerializationObservations = (Result<Option<serde_json::Value>, ServerError>, rpc::Message);

  /// Native paired lifecycle observations for both runtime token families.
  type FamilyLifecycles = (
    LifecycleObservations<super::LocalCancelToken>,
    LifecycleObservations<ConcurrentCancelToken>,
  );

  #[test]
  fn lifecycle_transitions_match_across_runtime_families() -> Result<(), Box<PredicateFailure<FamilyLifecycles>>> {
    ensure_that(
      (
        lifecycle_transitions::<LocalCancellation>(),
        lifecycle_transitions::<ConcurrentCancellation>(),
      ),
      "both runtime families must preserve every lifecycle transition and recovery",
      |observed| observed.0.preserves_transitions() && observed.1.preserves_transitions(),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn notification_guards_match_across_runtime_families() -> Result<(), Box<PredicateFailure<NotificationGuards>>> {
    ensure_that(
      [
        notification_guards::<LocalCancellation>(),
        notification_guards::<ConcurrentCancellation>(),
      ],
      "both runtime families must contain invalid and premature notifications",
      |observed| {
        observed
          .iter()
          .flatten()
          .all(|result| matches!(*result, Ok(NotificationDisposition::Handled)))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Intrinsic cancellation state before and after each cancellation effect.
  type CancellationStates = [[bool; 3]; 4];

  #[test]
  fn cancellation_is_runtime_honest_and_shared_semantically() -> Result<(), ComparisonFailure<CancellationStates, CancellationStates>> {
    let local = LocalCancellation::default();
    let mut local_token = local.token();
    let local_before = [
      local_token.is_cancelled(),
      local_token.is_terminated(),
      local_token.as_error().is_terminated(),
    ];
    local.cancel();
    let local_after = [
      local_token.is_cancelled(),
      local_token.is_terminated(),
      local_token.as_error().is_terminated(),
    ];
    let concurrent = ConcurrentCancellation::default();
    let mut concurrent_token = concurrent.token();
    let concurrent_before = [
      concurrent_token.is_cancelled(),
      concurrent_token.is_terminated(),
      concurrent_token.as_error().is_terminated(),
    ];
    concurrent.cancel();
    let concurrent_after = [
      concurrent_token.is_cancelled(),
      concurrent_token.is_terminated(),
      concurrent_token.as_error().is_terminated(),
    ];
    ensure_eq(
      [local_before, local_after, concurrent_before, concurrent_after],
      [[false; 3], [true; 3], [false; 3], [true; 3]],
      "cancellation must terminate native tokens and RPC adapters in both families",
    )
    .map(drop)
  }

  #[test]
  fn cancellation_registration_closes_the_check_register_race() -> Result<(), ComparisonFailure<CancellationRace, CancellationRace>> {
    let waiter = CancellationWaiter::register(LocalCancellationRegistry::default());
    let cancelled = Cell::new(false);
    let waker = noop_waker();
    let observed = poll_cancellation(&waiter, &Context::from_waker(&waker), || {
      let current = cancelled.get();
      cancelled.set(true);
      current
    });
    ensure_eq(
      (observed, cancelled.get()),
      (Poll::Ready(()), true),
      "a cancellation racing registration must be observed on the second flag read",
    )
    .map(drop)
  }

  #[test]
  fn cancellation_wakes_every_registered_token_clone() -> Result<(), ComparisonFailure<CancellationWakeFamilies, CancellationWakeFamilies>>
  {
    ensure_eq(
      [
        cancellation_wake_observations::<LocalCancellation>(false),
        cancellation_wake_observations::<ConcurrentCancellation>(false),
      ],
      [([Poll::Pending; 2], [1, 1]); 2],
      "cancellation must wake every independently registered clone",
    )
    .map(drop)
  }

  /// Native observations of dropped registrations and inactive identifiers.
  type CancellationFamilies = (
    [CancellationWakeObservations; 2],
    CancellationLookup<super::LocalCancelToken>,
    CancellationLookup<ConcurrentCancelToken>,
  );

  #[test]
  fn cancellation_drop_and_inactive_id_semantics_match_across_families() -> Result<(), Box<PredicateFailure<CancellationFamilies>>> {
    ensure_that(
      (
        [
          cancellation_wake_observations::<LocalCancellation>(true),
          cancellation_wake_observations::<ConcurrentCancellation>(true),
        ],
        cancellation_ignores_inactive_ids::<LocalCancellation>(),
        cancellation_ignores_inactive_ids::<ConcurrentCancellation>(),
      ),
      "dropping one waiter and cancelling inactive IDs must preserve other active tokens",
      |observed| {
        observed.0 == [([Poll::Pending; 2], [0, 1]); 2]
          && observed.1.requests.iter().all(Result::is_ok)
          && observed.2.requests.iter().all(Result::is_ok)
          && observed.1.polls == [[Poll::Pending; 2], [Poll::Ready(()), Poll::Pending]]
          && observed.2.polls == [[Poll::Pending; 2], [Poll::Ready(()), Poll::Pending]]
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn cancellation_error_adapters_return_the_standard_request_error()
  -> Result<(), Box<ComparisonFailure<CancellationErrors, CancellationErrors>>> {
    let local = LocalCancellation::default();
    let mut local_token = local.token();
    local.cancel();
    let concurrent = ConcurrentCancellation::default();
    let mut concurrent_token = concurrent.token();
    concurrent.cancel();
    ensure_eq(
      [block_on(local_token.as_error()), block_on(concurrent_token.as_error())],
      [Err(rpc::RpcError::request_cancelled()), Err(rpc::RpcError::request_cancelled())],
      "both adapters must return the complete standard request-cancelled RPC error",
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn mutation_ordering_matches_across_runtime_families() -> Result<(), Box<PredicateFailure<MutationFamilies>>> {
    ensure_that(
      [
        mutation_ordering::<LocalMutationStorage>(),
        mutation_ordering::<ConcurrentMutationStorage>(),
      ],
      "a later mutation cannot bypass an unfinished predecessor",
      |observed| {
        observed.iter().all(|barrier| {
          barrier.issued.iter().all(Result::is_ok)
            && barrier.polls == [Poll::Pending, Poll::Pending, Poll::Ready(())]
            && barrier.completed.is_empty()
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn mutation_waiter_lifecycle_matches_across_runtime_families() -> Result<(), Box<PredicateFailure<MutationFamilies>>> {
    ensure_that(
      [
        mutation_waiter_lifecycle::<LocalMutationStorage>(),
        mutation_waiter_lifecycle::<ConcurrentMutationStorage>(),
      ],
      "waiter replacement, completion, and drop must leave no stale wakes or registrations",
      |observed| {
        observed.iter().all(|barrier| {
          barrier.issued.iter().all(Result::is_ok)
            && barrier.polls == [Poll::Pending, Poll::Pending, Poll::Ready(()), Poll::Pending]
            && barrier.waiter_counts == [1, 0, 0]
            && barrier.wake_counts == [[0, 1], [0, 1]]
            && barrier.completed.is_empty()
        })
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn wire_classification_rejects_ambiguous_request_shapes() -> Result<(), Box<PredicateFailure<InvalidClassifications>>> {
    let null_id = message("fixture", rpc::MessageId::Null, None);
    let scalar = message(
      "fixture",
      rpc::MessageId::Value(NumberOrString::Number(1)),
      Some(serde_json::Value::Bool(true)),
    );
    let mut with_result = message("fixture", rpc::MessageId::Value(NumberOrString::Number(1)), None);
    with_result.result = Some(serde_json::Value::Null);
    let mut with_error = message("fixture", rpc::MessageId::Value(NumberOrString::Number(2)), None);
    with_error.error = Some(rpc::RpcError::internal_error());
    let mut response = response_message("2.0", 7, Some(serde_json::Value::Null), None);
    response.params = Some(serde_json::json!({"invalid": true}));
    let mut scalar_response = response_message("2.0", 8, Some(serde_json::Value::Null), None);
    scalar_response.params = Some(serde_json::Value::Bool(true));
    ensure_that(
      [null_id, scalar, with_result, with_error, response, scalar_response].map(classify_message),
      "ambiguous requests and parameter-bearing responses must retain their directional typed classifications",
      |observed| {
        matches!(*observed, [
          Inbound::InvalidRequest(_),
          Inbound::InvalidRequest(_),
          Inbound::InvalidRequest(_),
          Inbound::InvalidRequest(_),
          Inbound::InvalidResponse(ServerError::InvalidResponseShape {
            id: NumberOrString::Number(7),
          }),
          Inbound::InvalidResponse(ServerError::InvalidResponseShape {
            id: NumberOrString::Number(8),
          })
        ])
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn serialization_failures_retain_context_and_become_internal_responses() -> Result<(), Box<PredicateFailure<SerializationObservations>>> {
    ensure_that(
      (
        serialize_optional(Some(SerializationFailureFixture), "fixture request parameters"),
        outcome_message(
          NumberOrString::Number(8),
          RequestOutcome::SerializationFailure(String::from("fixture result cannot serialize")),
        ),
      ),
      "serialization boundaries must retain operation context and complete same-ID internal responses",
      |observed| {
        matches!(
          observed.0,
          Err(ServerError::Serialization {
            context: "fixture request parameters",
            ..
          })
        ) && observed.1.id == rpc::MessageId::Value(NumberOrString::Number(8))
          && observed.1.error == Some(rpc::RpcError::internal_error().with_details("fixture result cannot serialize"))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Captured scheduling results retaining the mutation ticket until the final check.
  type SchedulingObservations = [MessageSchedule<LocalMutationStorage>; 3];

  #[test]
  fn scheduling_captures_prior_mutations_before_requests() -> Result<(), Box<PredicateFailure<SchedulingObservations>>> {
    let barrier = MutationBarrier::<LocalMutationStorage>::default();
    let methods = HashSet::from(["mutate"]);
    let observed = [
      schedule_message(&message("mutate", rpc::MessageId::Missing, None), &methods, &barrier),
      schedule_message(
        &message("read", rpc::MessageId::Value(NumberOrString::Number(1)), None),
        &methods,
        &barrier,
      ),
      schedule_message(&message("observe", rpc::MessageId::Missing, None), &methods, &barrier),
    ];
    ensure_that(
      observed,
      "requests must capture prior mutation revisions while independent notifications bypass ordering",
      |outcomes| {
        matches!(*outcomes, [
          MessageSchedule::Mutation {
            ticket: Ok(_)
          },
          MessageSchedule::Request {
            prior_revision: 1
          },
          MessageSchedule::Independent
        ])
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Generate the same externally observable ordering and cancellation
  /// contracts for one runtime family.
  macro_rules! runtime_behavior_tests {
    (
      ordering = $ordering_test:ident,
      cancellation = $cancellation_test:ident,
      lifecycle = $lifecycle_test:ident,
      outbound_cleanup = $outbound_cleanup_test:ident,
      context = $context_test:ident,
      registry = $registry_test:ident,
      wire_lifecycle = $wire_lifecycle_test:ident,
      wire_requests = $wire_request_test:ident,
      wire_notifications = $wire_notification_test:ident,
      wire_responses = $wire_response_test:ident,
      outbound_results = $outbound_results_test:ident,
      outbound_errors = $outbound_errors_test:ident,
      outbound_cancellation = $outbound_cancellation_test:ident,
      server = $server:ident,
      initialize = $initialize:ident,
      initialize_runtime = $initialize_runtime:ident,
      initialized_fixture = $initialized_fixture:ident,
      outbound_value_fixture = $outbound_value_fixture:ident,
      fixture = $fixture:ident,
      read_fixture = $read_fixture:ident,
      read_request = $read_request:ident,
      outbound_value_exchange = $outbound_value_exchange:ident,
      mutation_message = $mutation_message:ident,
      mutation = $mutation:ident,
      read = $read:ident,
      pending = $pending:ident,
      outbound_probe = $outbound_probe:ident,
      context_probe = $context_probe:ident,
      value_probe = $value_probe:ident,
      cancel_probe = $cancel_probe:ident,
      world_type = $world_type:ty,
      expected = $expected:literal,
      deferred = $deferred:literal,
      family = $family:literal,
    ) => {
      /// Construct a runtime owner with its complete mutable observation record.
      fn $fixture() -> RuntimeObservations<$world_type, $server<$world_type>> {
        RuntimeObservations {
          world: <$world_type>::default(), writer: TestWriter::default(),
          server: $server::new().on_request::<InitializeRequest, _>($initialize),
          dispatch: Vec::new(), values: Vec::new(), messages: Vec::new(), registry: Vec::new(),
        }
      }

      /// Register scalar reads on the original runtime fixture.
      fn $read_fixture() -> RuntimeObservations<$world_type, $server<$world_type>> {
        let mut observed = $fixture();
        observed.server = observed.server.on_request::<GetValue, _>($read);
        observed
      }

      /// Retain the native completion of one public read request.
      fn $read_request(observed: &mut RuntimeObservations<$world_type, $server<$world_type>>, id: rpc::MessageId, params: Option<serde_json::Value>) {
        observed.dispatch.push((concat!("the ", $family, " read request must receive a protocol response"), block_on(observed.server.handle_message(Clone::clone(&observed.world), message(GetValue::METHOD, id, params), observed.writer.clone()))));
      }

      /// Construct the mutation used by ordering and lifecycle scenarios.
      fn $mutation_message() -> rpc::Message {
        message(SetValue::METHOD, rpc::MessageId::Missing, Some(serde_json::json!({"value": $expected})))
      }

      /// Retain initialization delivery together with its owning runtime fixture.
      fn $initialize_runtime(observed: &mut RuntimeObservations<$world_type, $server<$world_type>>, context: &'static str) {
        observed.dispatch.push((context, block_on(observed.server.handle_message(Clone::clone(&observed.world), message(InitializeRequest::METHOD, rpc::MessageId::Value(NumberOrString::Number(0)), None), observed.writer.clone()))));
      }

      /// Construct and initialize one registered request fixture without discarding native setup outcomes.
      macro_rules! $initialized_fixture {
        ($request:ty, $handler:ident, $context:expr) => {{
          let mut observed = $fixture();
          observed.server = observed.server.on_request::<$request, _>($handler);
          $initialize_runtime(&mut observed, $context);
          observed
        }};
      }

      /// Construct the typed outbound-response fixture.
      fn $outbound_value_fixture() -> RuntimeObservations<$world_type, $server<$world_type>> {
        $initialized_fixture!(OutboundValueProbe, $value_probe, concat!("the ", $family, " outbound-value server must initialize"))
      }

      /// Drive and retain both native halves of an outbound scalar exchange.
      fn $outbound_value_exchange(observed: &mut RuntimeObservations<$world_type, $server<$world_type>>, inbound_id: i32, response: rpc::Message) {
        observed.dispatch.extend(complete_exchange(
          observed.server.handle_message(Clone::clone(&observed.world), message(OutboundValueProbe::METHOD, rpc::MessageId::Value(NumberOrString::Number(inbound_id)), None), observed.writer.clone()),
          observed.server.handle_message(Clone::clone(&observed.world), response, observed.writer.clone()),
          concat!("the ", $family, " outbound value probe must complete"),
          concat!("the ", $family, " outbound value response must route to its waiter"),
        ));
      }

      /// Requests must observe every previously issued ordered mutation.
      #[test]
      fn $ordering_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $read_fixture();
        observed.server = observed.server.on_mutation_notification::<SetValue, _>($mutation);
        $initialize_runtime(&mut observed, concat!("the ", $family, " ordering server must initialize"));
        let mutation = observed.server.handle_message(Clone::clone(&observed.world), $mutation_message(), observed.writer.clone());
        let request = observed.server.handle_message(Clone::clone(&observed.world), message(GetValue::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None), observed.writer.clone());
        observed.dispatch.extend(complete_exchange(mutation, request, "the prior mutation must complete", "the dependent request must complete"));
        ensure_that(observed, concat!("the ", $family, " request must wait for and observe its prior mutation"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && state.writer.result_for(&rpc::MessageId::Value(NumberOrString::Number(1))) == Some(serde_json::json!($expected))).map(drop).map_err(Box::new)
      }

      /// Active cancellation must preserve the duplicate-ID rejection independently.
      #[test]
      fn $cancellation_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $initialized_fixture!(PendingRequest, $pending, concat!("the ", $family, " cancellation server must initialize"));
        let active = message(PendingRequest::METHOD, rpc::MessageId::Value(NumberOrString::Number(9)), None);
        let pending_request = observed.server.handle_message(Clone::clone(&observed.world), active.clone(), observed.writer.clone());
        let duplicate_request = observed.server.handle_message(Clone::clone(&observed.world), active, observed.writer.clone());
        let cancellation = observed.server.handle_message(Clone::clone(&observed.world), message(notification::Cancel::METHOD, rpc::MessageId::Missing, Some(serde_json::json!({"id": 9}))), observed.writer.clone());
        let (pending_result, duplicate_result, cancellation_result) = block_on(join3(pending_request, duplicate_request, cancellation));
        observed.dispatch.extend([("cancelled request", pending_result), ("duplicate request", duplicate_result), ("cancellation notification", cancellation_result)]);
        ensure_that(observed, concat!("the ", $family, " runtime must cancel the active request while rejecting its duplicate"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && response_error_codes(&state.writer, NumberOrString::Number(9)) == [-32800, -32600]).map(drop).map_err(Box::new)
      }

      /// Initialization, ordinary work, shutdown, and exit retain complete protocol results.
      #[test]
      fn $lifecycle_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $read_fixture();
        observed.server = observed.server.on_notification::<SetValue, _>($mutation);
        $initialize_runtime(&mut observed, concat!("the ", $family, " lifecycle server must initialize"));
        for (context, wire) in [
          ("initialized notification", $mutation_message()),
          ("ordinary request", message(GetValue::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None)),
          ("shutdown request", message(request::Shutdown::METHOD, rpc::MessageId::Value(NumberOrString::Number(2)), None)),
          ("terminal exit", message(notification::Exit::METHOD, rpc::MessageId::Missing, None)),
        ] {
          observed.dispatch.push((context, block_on(observed.server.handle_message(Clone::clone(&observed.world), wire, observed.writer.clone()))));
        }
        ensure_that(observed, concat!("the ", $family, " runtime must preserve complete lifecycle dispatch and sole request responses"), |state| {
          state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && block_on(state.server.is_shutting_down())
            && state.writer.messages().len() == 3
            && state.writer.result_for(&rpc::MessageId::Value(NumberOrString::Number(1))) == Some(serde_json::json!($expected))
            && state.writer.result_for(&rpc::MessageId::Value(NumberOrString::Number(2))) == Some(serde_json::Value::Null)
        }).map(drop).map_err(Box::new)
      }

      /// Deferred work follows successful response delivery and clean retry.
      #[test]
      fn $context_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $initialized_fixture!(ContextProbe, $context_probe, concat!("the ", $family, " context server must initialize"));
        observed.writer.fail.store(true, Ordering::SeqCst);
        observed.dispatch.push(("rejected context response", block_on(observed.server.handle_message(Clone::clone(&observed.world), message(ContextProbe::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None), observed.writer.clone()))));
        observed.values.push(observed.world.load_value());
        observed.writer.fail.store(false, Ordering::SeqCst);
        observed.dispatch.push(("recovered context response", block_on(observed.server.handle_message(Clone::clone(&observed.world), message(ContextProbe::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None), observed.writer.clone()))));
        ensure_that(observed, concat!("the ", $family, " runtime must preserve failed writes, defer no mutation on failure, and recover its original request"), |state| {
          matches!(state.dispatch.as_slice(), [(_, Ok(())), (_, Err(ServerError::Transport(_))), (_, Ok(()))])
            && state.values == [0] && state.world.load_value() == $deferred
            && state.writer.notification_parameter(ProbeNotification::METHOD, "observed") == Some(serde_json::json!(0))
            && state.writer.result_for(&rpc::MessageId::Value(NumberOrString::Number(1))) == Some(serde_json::json!(0))
        }).map(drop).map_err(Box::new)
      }

      /// Same-kind replacement preserves independent request and mutation classification.
      #[test]
      fn $registry_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $fixture();
        observed.server = $server::<$world_type>::default().on_notification::<SetValue, _>($mutation).on_mutation_notification::<SetValue, _>($mutation).on_request::<GetValue, _>($read).on_request::<GetValue, _>($read);
        observed.registry.push(format!("{:?}", observed.server));
        observed.server = observed.server.on_notification::<SetValue, _>($mutation);
        observed.registry.push(format!("{:?}", observed.server));
        ensure_that(observed, concat!("the ", $family, " registry must preserve kind-specific replacement and remove superseded mutation ordering"), |state| {
          state.registry.len() == 2 && state.registry.iter().zip([1, 0]).all(|(description, mutation_count)| description.contains("request_handler_count: 1") && description.contains("notification_handler_count: 1") && description.contains(&format!("mutation_method_count: {mutation_count}")))
        }).map(drop).map_err(Box::new)
      }

      /// Uninitialized and invalid-version requests retain distinct response errors.
      #[test]
      fn $wire_lifecycle_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $read_fixture();
        $read_request(&mut observed, rpc::MessageId::Value(NumberOrString::Number(20)), None);
        let mut invalid = message(GetValue::METHOD, rpc::MessageId::Value(NumberOrString::Number(21)), None);
        invalid.jsonrpc = "1.0".into();
        observed.dispatch.push(("invalid-version request", block_on(observed.server.handle_message(Clone::clone(&observed.world), invalid, observed.writer.clone()))));
        ensure_that(observed, concat!("the ", $family, " wire boundary must distinguish lifecycle and version errors"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && response_error_codes(&state.writer, NumberOrString::Number(20)) == [-32002] && response_error_codes(&state.writer, NumberOrString::Number(21)) == [-32600]).map(drop).map_err(Box::new)
      }

      /// Unknown methods and invalid parameters retain distinct protocol failures.
      #[test]
      fn $wire_request_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $read_fixture();
        $initialize_runtime(&mut observed, concat!("the ", $family, " wire-boundary server must initialize"));
        observed.dispatch.push(("unknown request", block_on(observed.server.handle_message(Clone::clone(&observed.world), message("fixture/unregisteredRequest", rpc::MessageId::Value(NumberOrString::Number(22)), None), observed.writer.clone()))));
        $read_request(&mut observed, rpc::MessageId::Value(NumberOrString::Number(23)), Some(serde_json::json!({})));
        ensure_that(observed, concat!("the ", $family, " wire boundary must distinguish method and parameter errors"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && response_error_codes(&state.writer, NumberOrString::Number(22)) == [-32601] && response_error_codes(&state.writer, NumberOrString::Number(23)) == [-32602]).map(drop).map_err(Box::new)
      }

      /// Invalid and unknown notifications have no world or output side effects.
      #[test]
      fn $wire_notification_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $fixture();
        observed.server = observed.server.on_mutation_notification::<SetValue, _>($mutation);
        $initialize_runtime(&mut observed, concat!("the ", $family, " notification server must initialize"));
        observed.messages.push(observed.writer.messages());
        for (context, wire) in [
          ("invalid notification", message(SetValue::METHOD, rpc::MessageId::Missing, Some(serde_json::json!({"value": "invalid"})))),
          ("unknown notification", message("fixture/unregisteredNotification", rpc::MessageId::Missing, None)),
        ] { observed.dispatch.push((context, block_on(observed.server.handle_message(Clone::clone(&observed.world), wire, observed.writer.clone())))); }
        ensure_that(observed, concat!("ignored ", $family, " notifications must preserve both world and protocol output"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && state.world.load_value() == 0 && state.messages == [state.writer.messages()]).map(drop).map_err(Box::new)
      }

      /// Null-ID requests and malformed or unknown responses retain routing direction.
      #[test]
      fn $wire_response_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $read_fixture();
        $read_request(&mut observed, rpc::MessageId::Null, None);
        observed.messages.push(observed.writer.messages());
        observed.dispatch.push(("invalid response version", block_on(observed.server.handle_message(Clone::clone(&observed.world), response_message("1.0", 99, Some(serde_json::Value::Null), None), observed.writer.clone()))));
        observed.dispatch.push(("unknown response ID", block_on(observed.server.handle_message(Clone::clone(&observed.world), response_message("2.0", 99, Some(serde_json::Value::Null), None), observed.writer.clone()))));
        ensure_that(observed, concat!("the ", $family, " wire boundary must preserve null request IDs and native response failures"), |state| {
          matches!(state.dispatch.as_slice(), [(_, Ok(())), (_, Err(ServerError::InvalidResponseVersion { id: NumberOrString::Number(99), version })), (_, Ok(()))] if version == "1.0")
            && state.messages.iter().flatten().any(|wire| wire.id == rpc::MessageId::Null && wire.error.as_ref().is_some_and(|error| error.code == -32600))
            && state.messages == [state.writer.messages()]
        }).map(drop).map_err(Box::new)
      }

      /// Outbound typed values and deserialization errors reach the matching inbound requests.
      #[test]
      fn $outbound_results_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $outbound_value_fixture();
        $outbound_value_exchange(&mut observed, 10, response_message("2.0", 0, Some(serde_json::json!(88)), None));
        $outbound_value_exchange(&mut observed, 11, response_message("2.0", 1, Some(serde_json::json!("not-a-number")), None));
        ensure_that(observed, concat!("the ", $family, " outbound router must preserve typed success and deserialization failure"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && state.writer.result_for(&rpc::MessageId::Value(NumberOrString::Number(10))) == Some(serde_json::json!(88)) && response_error_codes(&state.writer, NumberOrString::Number(11)) == [-32603] && state.writer.method_count(OutboundValueRequest::METHOD) == 2).map(drop).map_err(Box::new)
      }

      /// Outbound RPC errors survive and valid unknown responses remain inert.
      #[test]
      fn $outbound_errors_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $outbound_value_fixture();
        $outbound_value_exchange(&mut observed, 12, response_message("2.0", 0, None, Some(rpc::RpcError::method_not_found())));
        observed.dispatch.push(("unknown valid response", block_on(observed.server.handle_message(Clone::clone(&observed.world), response_message("2.0", 100, Some(serde_json::json!(5)), None), observed.writer.clone()))));
        ensure_that(observed, concat!("the ", $family, " outbound router must preserve RPC errors and ignore unknown responses"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && response_error_codes(&state.writer, NumberOrString::Number(12)) == [-32601] && state.writer.method_count(OutboundValueRequest::METHOD) == 1).map(drop).map_err(Box::new)
      }

      /// Cancellation names the outbound request that owns the pending response.
      #[test]
      fn $outbound_cancellation_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $initialized_fixture!(OutboundCancelProbe, $cancel_probe, concat!("the ", $family, " outbound-cancellation server must initialize"));
        observed.dispatch.extend(complete_exchange(
          observed.server.handle_message(Clone::clone(&observed.world), message(OutboundCancelProbe::METHOD, rpc::MessageId::Value(NumberOrString::Number(13)), None), observed.writer.clone()),
          observed.server.handle_message(Clone::clone(&observed.world), response_message("2.0", 0, Some(serde_json::json!(1)), None), observed.writer.clone()),
          "outbound cancellation probe", "cancelled outbound response",
        ));
        ensure_that(observed, concat!("the ", $family, " cancellation must preserve the active outbound request identity"), |state| state.dispatch.iter().all(|&(_, ref outcome)| outcome.is_ok()) && state.writer.notification_parameter(notification::Cancel::METHOD, "id") == Some(serde_json::json!(0)) && state.writer.method_count(OutboundValueRequest::METHOD) == 1).map(drop).map_err(Box::new)
      }

      /// Malformed responses retire the response channel before later cancellation.
      #[test]
      fn $outbound_cleanup_test() -> RuntimeAssertion<$world_type, $server<$world_type>> {
        let mut observed = $initialized_fixture!(OutboundProbe, $outbound_probe, concat!("the ", $family, " outbound-response server must initialize"));
        observed.dispatch.extend(complete_exchange(
          observed.server.handle_message(Clone::clone(&observed.world), message(OutboundProbe::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None), observed.writer.clone()),
          observed.server.handle_message(Clone::clone(&observed.world), response_message("2.0", 0, None, None), observed.writer.clone()),
          "closed response channel recovery", "malformed outbound response",
        ));
        ensure_that(observed, concat!("the ", $family, " context must complete recovery and forget its closed cancellation target"), |state| {
          matches!(state.dispatch.as_slice(), [(_, Ok(())), (_, Ok(())), (_, Err(ServerError::InvalidResponseShape { id: NumberOrString::Number(0) }))])
            && state.writer.messages().iter().any(|wire| wire.method.as_deref() == Some(OutboundRequest::METHOD) && wire.id == rpc::MessageId::Value(NumberOrString::Number(0)))
            && state.writer.messages().iter().any(|wire| wire.id == rpc::MessageId::Value(NumberOrString::Number(1)) && wire.result == Some(serde_json::Value::Null))
            && state.writer.method_count(notification::Cancel::METHOD) == 0
        }).map(drop).map_err(Box::new)
      }
    };
  }

  /// Instantiate one runtime-family behavior contract from positional matrix values.
  macro_rules! instantiate_runtime_behavior_tests {
    (
      $ordering:ident =>
      $cancellation:ident =>
      $lifecycle:ident =>
      $outbound_cleanup:ident =>
      $context:ident =>
      $registry:ident =>
      $wire_lifecycle:ident =>
      $wire_requests:ident =>
      $wire_notifications:ident =>
      $wire_responses:ident =>
      $outbound_results:ident =>
      $outbound_errors:ident =>
      $outbound_cancellation:ident =>
      $server:ident =>
      $initialize:ident =>
      $initialize_runtime:ident =>
      $initialized_fixture:ident =>
      $outbound_value_fixture:ident =>
      $fixture:ident =>
      $read_fixture:ident =>
      $read_request:ident =>
      $outbound_value_exchange:ident =>
      $mutation_message:ident =>
      $mutation:ident =>
      $read:ident =>
      $pending:ident =>
      $outbound_probe:ident =>
      $context_probe:ident =>
      $value_probe:ident =>
      $cancel_probe:ident =>
      $world_type:ty =>
      $expected:literal =>
      $deferred:literal =>
      $family:literal
    ) => {
      runtime_behavior_tests!(
        ordering = $ordering,
        cancellation = $cancellation,
        lifecycle = $lifecycle,
        outbound_cleanup = $outbound_cleanup,
        context = $context,
        registry = $registry,
        wire_lifecycle = $wire_lifecycle,
        wire_requests = $wire_requests,
        wire_notifications = $wire_notifications,
        wire_responses = $wire_responses,
        outbound_results = $outbound_results,
        outbound_errors = $outbound_errors,
        outbound_cancellation = $outbound_cancellation,
        server = $server,
        initialize = $initialize,
        initialize_runtime = $initialize_runtime,
        initialized_fixture = $initialized_fixture,
        outbound_value_fixture = $outbound_value_fixture,
        fixture = $fixture,
        read_fixture = $read_fixture,
        read_request = $read_request,
        outbound_value_exchange = $outbound_value_exchange,
        mutation_message = $mutation_message,
        mutation = $mutation,
        read = $read,
        pending = $pending,
        outbound_probe = $outbound_probe,
        context_probe = $context_probe,
        value_probe = $value_probe,
        cancel_probe = $cancel_probe,
        world_type = $world_type,
        expected = $expected,
        deferred = $deferred,
        family = $family,
      );
    };
  }

  /// Select one column from a paired local/concurrent behavior matrix.
  macro_rules! select_runtime_behavior_family {
    (local; $callback:ident; ($(($local:tt, $concurrent:tt)),+ $(,)?)) => {
      $callback!($($local)=>+);
    };
    (concurrent; $callback:ident; ($(($local:tt, $concurrent:tt)),+ $(,)?)) => {
      $callback!($($concurrent)=>+);
    };
  }

  /// Expand both runtime columns from one field-aligned contract matrix.
  macro_rules! runtime_behavior_matrix {
    ($($pair:tt),+ $(,)?) => {
      runtime_behavior_matrix!(@select [local, concurrent]; ($($pair),+));
    };
    (@select [$($selector:ident),+]; $pairs:tt) => {
      $(
        select_runtime_behavior_family!(
          $selector;
          instantiate_runtime_behavior_tests;
          $pairs
        );
      )+
    };
  }

  runtime_behavior_matrix!(
    (
      local_requests_observe_every_prior_mutation,
      concurrent_requests_observe_every_prior_mutation
    ),
    (
      local_cancellation_preserves_duplicate_id_state,
      concurrent_cancellation_preserves_duplicate_id_state
    ),
    (
      local_public_lifecycle_dispatches_registered_work,
      concurrent_public_lifecycle_dispatches_registered_work
    ),
    (
      local_closed_response_clears_cancellation_target,
      concurrent_closed_response_clears_cancellation_target
    ),
    (
      local_context_defers_only_after_response_and_recovers,
      concurrent_context_defers_only_after_response_and_recovers
    ),
    (
      local_registry_replacement_preserves_kind_and_ordering,
      concurrent_registry_replacement_preserves_kind_and_ordering
    ),
    (
      local_wire_boundary_rejects_uninitialized_and_invalid_versions,
      concurrent_wire_boundary_rejects_uninitialized_and_invalid_versions
    ),
    (
      local_wire_boundary_distinguishes_method_and_parameter_failures,
      concurrent_wire_boundary_distinguishes_method_and_parameter_failures
    ),
    (
      local_wire_boundary_ignores_invalid_and_unknown_notifications,
      concurrent_wire_boundary_ignores_invalid_and_unknown_notifications
    ),
    (
      local_wire_boundary_validates_and_routes_responses,
      concurrent_wire_boundary_validates_and_routes_responses
    ),
    (
      local_outbound_routing_preserves_typed_results,
      concurrent_outbound_routing_preserves_typed_results
    ),
    (
      local_outbound_routing_preserves_rpc_errors,
      concurrent_outbound_routing_preserves_rpc_errors
    ),
    (
      local_outbound_cancellation_targets_active_waiter,
      concurrent_outbound_cancellation_targets_active_waiter
    ),
    (LocalServer, ConcurrentServer),
    (initialize_runtime_context, initialize_runtime_context),
    (initialize_local_runtime, initialize_concurrent_runtime),
    (initialized_local_request_fixture, initialized_concurrent_request_fixture),
    (
      initialized_local_outbound_value_fixture,
      initialized_concurrent_outbound_value_fixture
    ),
    (local_runtime_fixture, concurrent_runtime_fixture),
    (local_read_runtime_fixture, concurrent_read_runtime_fixture),
    (dispatch_local_read_request, dispatch_concurrent_read_request),
    (complete_local_outbound_value_exchange, complete_concurrent_outbound_value_exchange),
    (local_set_value_message, concurrent_set_value_message),
    (set_local_value, set_concurrent_value),
    (get_local_value, get_concurrent_value),
    (pending_local, pending_concurrent),
    (probe_local_outbound_cleanup, probe_concurrent_outbound_cleanup),
    (probe_local_context, probe_concurrent_context),
    (probe_local_outbound_value, probe_concurrent_outbound_value),
    (probe_local_outbound_cancellation, probe_concurrent_outbound_cancellation),
    (LocalValue, ConcurrentValue),
    (41, 73),
    (91, 97),
    ("local", "concurrent"),
  );

  #[test]
  fn writer_io_failure_retains_its_original_boxed_source() -> Result<(), PredicateFailure<MessageWriterError>> {
    ensure_that(
      MessageWriterError::from(io::Error::from(io::ErrorKind::PermissionDenied)),
      "the writer category and original boxed source must retain permission denial",
      |error| {
        error.kind() == io::ErrorKind::PermissionDenied
          && matches!(*error, MessageWriterError::Io { ref source } if source.kind() == io::ErrorKind::PermissionDenied)
      },
    )
    .map(drop)
  }

  /// Idle writer and native close completion.
  type ClosedWriter = (TestWriter, Result<(), MessageWriterError>);

  #[test]
  fn discrete_message_writers_close_without_buffered_work() -> Result<(), PredicateFailure<ClosedWriter>> {
    let mut writer = TestWriter::default();
    let closed = block_on(futures::SinkExt::close(&mut writer));
    ensure_that(
      (writer, closed),
      "closing an idle discrete writer must succeed without fabricating output",
      |observed| observed.1.is_ok() && observed.0.messages().is_empty(),
    )
    .map(drop)
  }

  #[test]
  fn writer_failure_aborts_initialization_for_a_clean_retry() -> RuntimeAssertion<LocalValue, LocalServer<LocalValue>> {
    let mut observed = local_runtime_fixture();
    observed.writer = TestWriter::failing();
    initialize_local_runtime(&mut observed, "failed initialization response");
    observed.writer.fail.store(false, Ordering::SeqCst);
    initialize_local_runtime(&mut observed, "recovered initialization response");
    ensure_that(
      observed,
      "a failed initialize write must leave the lifecycle ready to retry its original request",
      |state| {
        matches!(state.dispatch.as_slice(), [(_, Err(ServerError::Transport(_))), (_, Ok(()))])
          && state
            .writer
            .messages()
            .iter()
            .any(|wire| wire.result == Some(serde_json::Value::Null) && wire.error.is_none())
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native thread spawn, panic, and server-completion layers.
  type RequestThread = Result<thread::Result<Result<(), ServerError>>, io::Error>;

  /// Native initialization, thread creation/join, and entry-channel results.
  #[derive(Debug)]
  struct OverlapObservations {
    /// Initialization delivery outcome before launching request threads.
    initialized: Result<(), ServerError>,
    /// Both spawn and join layers, including original panic payloads if a worker panics.
    threads:     [RequestThread; 2],
    /// Entry notifications observed before releasing either handler.
    starts:      [Result<(), mpsc::RecvTimeoutError>; 2],
    /// Output owner retaining complete messages from both request threads.
    writer:      TestWriter,
  }

  #[test]
  fn independent_concurrent_requests_overlap_on_separate_threads() -> Result<(), Box<PredicateFailure<OverlapObservations>>> {
    let (started, observations) = mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let world = OverlapWorld {
      started,
      release: Arc::clone(&release),
    };
    let writer = TestWriter::default();
    let server = ConcurrentServer::new()
      .on_request::<InitializeRequest, _>(initialize_overlap)
      .on_request::<OverlapRequest, _>(overlap_handler);
    let initialized = block_on(server.handle_message(
      world.clone(),
      message(InitializeRequest::METHOD, rpc::MessageId::Value(NumberOrString::Number(0)), None),
      writer.clone(),
    ));
    let shared_server = Arc::new(server);
    let first_server = Arc::clone(&shared_server);
    let first_world = world.clone();
    let first_writer = writer.clone();
    let first = Builder::new().name("first-overlapping-request".to_owned()).spawn(move || {
      block_on(first_server.handle_message(
        first_world,
        message(OverlapRequest::METHOD, rpc::MessageId::Value(NumberOrString::Number(1)), None),
        first_writer,
      ))
    });
    let second_writer = writer.clone();
    let second = Builder::new().name("second-overlapping-request".to_owned()).spawn(move || {
      block_on(shared_server.handle_message(
        world,
        message(OverlapRequest::METHOD, rpc::MessageId::Value(NumberOrString::Number(2)), None),
        second_writer,
      ))
    });
    let starts = [
      observations.recv_timeout(Duration::from_secs(2)),
      observations.recv_timeout(Duration::from_secs(2)),
    ];
    release.store(true, Ordering::SeqCst);
    let threads = [first.map(thread::JoinHandle::join), second.map(thread::JoinHandle::join)];
    ensure_that(
      OverlapObservations {
        initialized,
        threads,
        starts,
        writer,
      },
      "both native handlers must enter before release and preserve successful spawn, join, and response delivery",
      |observed| {
        observed.initialized.is_ok()
          && observed.starts.iter().all(Result::is_ok)
          && observed.threads.iter().all(|outcome| matches!(*outcome, Ok(Ok(Ok(())))))
          && observed.writer.messages().len() == 3
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn concurrent_protocol_values_are_send_and_sync() {
    require_send_sync::<ConcurrentCancelToken>();
    require_send_sync::<ConcurrentServer<()>>();
  }
}
