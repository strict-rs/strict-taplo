//! Tokio-backed native LSP transports for [`crate::ConcurrentServer`].

use std::future::Future;
use std::future::poll_fn;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures::Sink;
use futures::SinkExt as _;
use futures::Stream;
use futures::StreamExt as _;
use futures::channel::mpsc::SendError;
use futures::channel::mpsc::UnboundedReceiver;
use futures::channel::mpsc::UnboundedSender;
use futures::channel::mpsc::unbounded;
#[cfg(feature = "tokio-tcp")]
use futures::future::Either;
#[cfg(feature = "tokio-tcp")]
use futures::future::select;
use lsp_types::notification;
use lsp_types::notification::Notification as _;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt as _;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt as _;
use tokio::io::BufReader;
#[cfg(feature = "tokio-tcp")]
use tokio::net::TcpListener;
#[cfg(feature = "tokio-tcp")]
use tokio::net::ToSocketAddrs;
use tokio::runtime::Handle;
use tokio::task::JoinError;
use tokio::task::JoinHandle;
use tokio::task::JoinSet;

use crate::ConcurrentServer;
use crate::MessageWriterError;
use crate::ServerError;
use crate::rpc;

/// Maximum accepted JSON-RPC body length.
const MAXIMUM_MESSAGE_LENGTH: usize = 67_108_864;

impl<W> ConcurrentServer<W>
where
  W: Clone + Send + Sync + 'static,
{
  /// Listen for LSP messages over standard input and output.
  ///
  /// # Errors
  ///
  /// Returns [`ServerError`] when framing, transport, handler execution, or lifecycle validation
  /// fails.
  #[cfg(feature = "tokio-stdio")]
  pub async fn listen_stdio<I, O, S>(self, world: W, input_stream: I, output_stream: O, shutdown_signals: S) -> Result<(), ServerError>
  where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    S: Stream<Item = ()> + Unpin + Send + 'static,
  {
    listen_stdio(self, world, input_stream, output_stream, shutdown_signals).await
  }

  /// Listen for one LSP client over TCP.
  ///
  /// # Errors
  ///
  /// Returns [`ServerError`] when binding, accepting, framing, transport, handler execution, or
  /// lifecycle validation fails.
  #[cfg(feature = "tokio-tcp")]
  pub async fn listen_tcp<A, S>(self, world: W, address: A, shutdown_signals: S) -> Result<(), ServerError>
  where
    A: ToSocketAddrs + Send,
    S: Stream<Item = ()> + Unpin + Send + 'static,
  {
    listen_tcp(self, world, address, shutdown_signals).await
  }
}

/// One successfully framed inbound transport event.
#[derive(Debug)]
enum InputEvent {
  /// A decoded JSON-RPC message ready for server classification.
  Message(rpc::Message),
  /// A JSON-RPC parse or invalid-request response produced during decoding.
  ProtocolError(rpc::Message),
}

/// Result returned by one owned native transport task.
type TransportTaskResult = Result<(), ServerError>;

/// Join handle for one owned native transport task.
type TransportTask = JoinHandle<TransportTaskResult>;

/// Framed input channel and its producer task.
type InputTransport = (UnboundedReceiver<InputEvent>, TransportTask);

/// Serialized output channel and its consumer task.
type OutputTransport = (UnboundedSender<rpc::Message>, TransportTask);

/// Lifecycle of one native transport task observed by the arbiter.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TaskCompletion {
  /// The task can still produce its final typed result.
  Active,
  /// The task's final typed result has been interpreted.
  Complete,
}

/// Lifecycle of the external process-stop stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownSignalState {
  /// The stream can still yield an external stop request.
  Open,
  /// The stream ended without requesting a stop.
  Closed,
  /// The stream yielded an external stop request.
  Requested,
}

/// Complete native transport resources owned by one listen invocation.
struct NativeTransport<S> {
  /// Runtime used for independently executing message handlers.
  runtime:          Handle,
  /// Successfully framed input events.
  input:            UnboundedReceiver<InputEvent>,
  /// Input framing task.
  input_task:       TransportTask,
  /// Serialized output queue.
  output:           UnboundedSender<rpc::Message>,
  /// Output framing task.
  output_task:      TransportTask,
  /// External process-stop signals.
  shutdown_signals: S,
  /// Current external process-stop stream state.
  shutdown_state:   ShutdownSignalState,
  /// Independently executing message-handler tasks.
  handlers:         JoinSet<TransportTaskResult>,
  /// Framed-reader task completion state.
  reader_state:     TaskCompletion,
  /// Serialized-writer task completion state.
  writer_state:     TaskCompletion,
}

/// Poll one active transport task and record exactly one completion transition.
fn poll_transport_task(
  task_role: &'static str,
  task_handle: &mut TransportTask,
  completion: &mut TaskCompletion,
  context: &mut Context<'_>,
) -> Poll<Result<(), ServerError>> {
  if *completion == TaskCompletion::Complete {
    return Poll::Pending;
  }
  match Pin::new(task_handle).poll(context) {
    Poll::Ready(result) => {
      *completion = TaskCompletion::Complete;
      Poll::Ready(joined_task_result(task_role, result, false))
    }
    Poll::Pending => Poll::Pending,
  }
}

/// Drain every currently completed handler and retain the first typed failure.
#[allow(
  clippy::single_call_fn,
  reason = "handler draining remains distinct so transport polling surfaces completed task failures before accepting more input"
)]
fn poll_handler_completions(handlers: &mut JoinSet<TransportTaskResult>, context: &mut Context<'_>) -> Result<(), ServerError> {
  loop {
    match handlers.poll_join_next(context) {
      Poll::Ready(Some(result)) => joined_task_result("handler", result, false)?,
      Poll::Ready(None) | Poll::Pending => return Ok(()),
    }
  }
}

impl<S> NativeTransport<S> {
  /// Combine runtime ownership with already-created input and output tasks.
  fn new(
    runtime: Handle,
    input: InputTransport,
    output: OutputTransport,
    shutdown_signals: S,
    shutdown_state: ShutdownSignalState,
  ) -> Self {
    Self {
      runtime,
      input: input.0,
      input_task: input.1,
      output: output.0,
      output_task: output.1,
      shutdown_signals,
      shutdown_state,
      handlers: JoinSet::new(),
      reader_state: TaskCompletion::Active,
      writer_state: TaskCompletion::Active,
    }
  }

  /// Poll the external stop stream until it requests shutdown or becomes inert.
  fn poll_shutdown(&mut self, context: &mut Context<'_>) -> Poll<()>
  where
    S: Stream<Item = ()> + Unpin,
  {
    if self.shutdown_state != ShutdownSignalState::Open {
      return Poll::Pending;
    }
    match Pin::new(&mut self.shutdown_signals).poll_next(context) {
      Poll::Ready(Some(())) => {
        self.shutdown_state = ShutdownSignalState::Requested;
        Poll::Ready(())
      }
      Poll::Ready(None) => {
        self.shutdown_state = ShutdownSignalState::Closed;
        Poll::Pending
      }
      Poll::Pending => Poll::Pending,
    }
  }

  /// Poll control-plane and protocol work until input or transport completion.
  fn poll_next_input(&mut self, context: &mut Context<'_>) -> Poll<Result<Option<InputEvent>, ServerError>>
  where
    S: Stream<Item = ()> + Unpin,
  {
    if self.poll_shutdown(context).is_ready() {
      return Poll::Ready(Ok(None));
    }

    if let Poll::Ready(Err(error)) = poll_transport_task("input", &mut self.input_task, &mut self.reader_state, context) {
      return Poll::Ready(Err(error));
    }

    if let Poll::Ready(result) = poll_transport_task("output", &mut self.output_task, &mut self.writer_state, context) {
      return Poll::Ready(result.map(|()| None));
    }

    if let Err(error) = poll_handler_completions(&mut self.handlers, context) {
      return Poll::Ready(Err(error));
    }

    match Pin::new(&mut self.input).poll_next(context) {
      Poll::Ready(event) => Poll::Ready(Ok(event)),
      Poll::Pending => Poll::Pending,
    }
  }

  /// Await the next framed input event or native transport completion.
  async fn next_input(&mut self) -> Result<Option<InputEvent>, ServerError>
  where
    S: Stream<Item = ()> + Unpin,
  {
    poll_fn(|context| self.poll_next_input(context)).await
  }
}

/// Listen for LSP messages over standard input and output.
///
/// # Errors
///
/// Returns [`ServerError`] when runtime discovery, framing, transport, handler execution, or
/// lifecycle validation fails.
#[cfg(feature = "tokio-stdio")]
#[allow(
  clippy::single_call_fn,
  reason = "the prepared stdio adapter separates stream setup and runtime ownership from the public server method"
)]
async fn listen_stdio<W, I, O, S>(
  server: ConcurrentServer<W>,
  world: W,
  input_stream: I,
  output_stream: O,
  shutdown_signals: S,
) -> Result<(), ServerError>
where
  W: Clone + Send + Sync + 'static,
  I: AsyncRead + Unpin + Send + 'static,
  O: AsyncWrite + Unpin + Send + 'static,
  S: Stream<Item = ()> + Unpin + Send + 'static,
{
  let runtime = runtime_handle()?;
  let input = create_input(&runtime, input_stream);
  let output = create_output(&runtime, output_stream);
  tracing::info!(transport = "stdio", "LSP server listening");
  drive_server(
    server,
    world,
    NativeTransport::new(runtime, input, output, shutdown_signals, ShutdownSignalState::Open),
  )
  .await
}

/// Accept one TCP value while preserving the residual stop-stream state.
#[cfg(feature = "tokio-tcp")]
#[allow(
  clippy::single_call_fn,
  reason = "TCP accept arbitration preserves whether the shutdown stream closed while an accept was pending"
)]
async fn accept_or_shutdown<S, A, T, E>(shutdown_signals: &mut S, accept: A) -> Result<Option<(T, ShutdownSignalState)>, E>
where
  S: Stream<Item = ()> + Unpin,
  A: Future<Output = Result<T, E>>,
{
  let pending_shutdown = Box::pin(shutdown_signals.next());
  let pending_accept = Box::pin(accept);
  match select(pending_shutdown, pending_accept).await {
    Either::Left((Some(()), residual_accept)) => {
      drop(residual_accept);
      Ok(None)
    }
    Either::Left((None, residual_accept)) => residual_accept
      .await
      .map(|accepted| Some((accepted, ShutdownSignalState::Closed))),
    Either::Right((accepted, residual_shutdown)) => {
      drop(residual_shutdown);
      accepted.map(|accepted_transport| Some((accepted_transport, ShutdownSignalState::Open)))
    }
  }
}

/// Listen for one LSP client over TCP.
///
/// # Errors
///
/// Returns [`ServerError`] when runtime discovery, binding, accepting, framing, transport, handler
/// execution, or lifecycle validation fails.
#[cfg(feature = "tokio-tcp")]
#[allow(
  clippy::single_call_fn,
  reason = "the prepared TCP adapter owns binding, first-client acceptance, and shutdown-state handoff"
)]
async fn listen_tcp<W, A, S>(server: ConcurrentServer<W>, world: W, address: A, mut shutdown_signals: S) -> Result<(), ServerError>
where
  W: Clone + Send + Sync + 'static,
  A: ToSocketAddrs + Send,
  S: Stream<Item = ()> + Unpin + Send + 'static,
{
  let runtime = runtime_handle()?;
  let listener = TcpListener::bind(address).await?;
  let local_address = listener.local_addr()?;
  tracing::info!(transport = "tcp", address = ?local_address, "LSP server listening");

  let Some(((stream, client_address), shutdown_state)) = accept_or_shutdown(&mut shutdown_signals, listener.accept()).await? else {
    return Ok(());
  };
  tracing::info!(?client_address, "LSP client connected");
  let (reader, writer) = stream.into_split();
  let input = create_input(&runtime, reader);
  let output = create_output(&runtime, writer);
  drive_server(
    server,
    world,
    NativeTransport::new(runtime, input, output, shutdown_signals, shutdown_state),
  )
  .await
}

/// Drive the concurrent server over prepared input and output tasks.
async fn drive_server<W, S>(server: ConcurrentServer<W>, world: W, mut transport: NativeTransport<S>) -> Result<(), ServerError>
where
  W: Clone + Send + Sync + 'static,
  S: Stream<Item = ()> + Unpin + Send,
{
  let mut transport_result = loop {
    let next_input = match transport.next_input().await {
      Ok(next_input) => next_input,
      Err(error) => break Err(error),
    };
    let Some(input) = next_input else {
      break Ok(());
    };
    match input {
      InputEvent::ProtocolError(response) => {
        if let Err(error) = transport.output.unbounded_send(response) {
          break Err(ServerError::Transport(output_channel_error(error.into_send_error())));
        }
      }
      InputEvent::Message(wire_message) => {
        let terminal_exit = is_exit_notification(&wire_message);
        let handler = server.handle_message(world.clone(), wire_message, message_sink(transport.output.clone()));
        if terminal_exit {
          break handler.await;
        }
        let handler_id = transport.handlers.spawn_on(handler, &transport.runtime).id();
        tracing::trace!(?handler_id, "spawned LSP handler task");
      }
    }
  };

  if transport.reader_state == TaskCompletion::Active {
    transport.input_task.abort();
    preserve_primary(
      &mut transport_result,
      joined_task_result("input", (&mut transport.input_task).await, true),
    );
  }
  transport.handlers.abort_all();
  while let Some(result) = transport.handlers.join_next().await {
    preserve_primary(&mut transport_result, joined_task_result("handler", result, true));
  }
  let externally_stopped = transport.shutdown_state == ShutdownSignalState::Requested;
  drop(transport.output);
  if transport.writer_state == TaskCompletion::Active {
    preserve_primary(
      &mut transport_result,
      joined_task_result("output", (&mut transport.output_task).await, false),
    );
  }
  transport_result?;

  termination_result(server.is_shutting_down().await, externally_stopped)
}

/// Return whether a wire message is the terminal LSP `exit` notification.
#[allow(
  clippy::single_call_fn,
  reason = "terminal-exit recognition isolates the exact protocol condition that stops transport input"
)]
fn is_exit_notification(message: &rpc::Message) -> bool {
  message.is_notification() && message.method.as_deref() == Some(notification::Exit::METHOD)
}

/// Validate why the transport loop stopped.
#[allow(
  clippy::single_call_fn,
  reason = "termination validation isolates the invariant distinguishing graceful shutdown from premature exit"
)]
const fn termination_result(protocol_shutdown: bool, externally_stopped: bool) -> Result<(), ServerError> {
  if protocol_shutdown || externally_stopped {
    Ok(())
  } else {
    Err(ServerError::ExitBeforeShutdown)
  }
}

/// Preserve an established transport failure while accepting later teardown results.
fn preserve_primary(primary: &mut Result<(), ServerError>, cleanup: Result<(), ServerError>) {
  if primary.is_ok() && cleanup.is_err() {
    *primary = cleanup;
  }
}

/// Convert a closed output channel into the message-writer error contract.
fn output_channel_error(source: SendError) -> MessageWriterError {
  MessageWriterError::OutputChannelClosed {
    source: Box::new(source)
  }
}

/// Convert the futures channel sender into the server's writer-error contract.
#[allow(
  clippy::single_call_fn,
  reason = "the sink adapter centralizes output-channel error mapping at the handler dispatch boundary"
)]
fn message_sink(output: UnboundedSender<rpc::Message>) -> impl Sink<rpc::Message, Error = MessageWriterError> + Clone + Send + Unpin {
  output.sink_map_err(output_channel_error)
}

/// Interpret one completed native transport or handler task.
fn joined_task_result(
  task: &'static str,
  result: Result<TransportTaskResult, JoinError>,
  allow_cancelled: bool,
) -> Result<(), ServerError> {
  match result {
    Ok(task_result) => task_result,
    Err(error) if allow_cancelled && error.is_cancelled() => Ok(()),
    Err(source) => Err(ServerError::TransportTaskTerminated {
      task,
      source,
    }),
  }
}

/// Start the serialized output task.
fn create_output(runtime: &Handle, sink: impl AsyncWrite + Unpin + Send + 'static) -> OutputTransport {
  let (sender, mut receiver) = unbounded::<rpc::Message>();
  let task = runtime.spawn(async move {
    let mut output = sink;
    while let Some(message) = receiver.next().await {
      write_message(&mut output, message).await?;
    }
    output.flush().await.map_err(ServerError::Io)
  });
  (sender, task)
}

/// Serialize and frame one JSON-RPC message.
#[allow(
  clippy::single_call_fn,
  reason = "wire framing isolates JSON serialization, Content-Length construction, and ordered output flushing"
)]
async fn write_message<T: AsyncWrite + Unpin>(output: &mut T, message: rpc::Message) -> Result<(), ServerError> {
  let body = serde_json::to_vec(&message).map_err(|source| ServerError::Serialization {
    context: "JSON-RPC wire message",
    source,
  })?;
  let header = format!("Content-Length: {}\r\n\r\n", body.len());
  output.write_all(header.as_bytes()).await?;
  output.write_all(&body).await?;
  output.flush().await?;
  Ok(())
}

/// Start the framed input task.
fn create_input(runtime: &Handle, stream: impl AsyncRead + Unpin + Send + 'static) -> InputTransport {
  let (sender, receiver) = unbounded();
  let task = runtime.spawn(async move {
    let mut input = BufReader::new(stream);
    while let Some(message) = read_message(&mut input).await? {
      if sender.unbounded_send(message).is_err() {
        return Ok(());
      }
    }
    Ok(())
  });
  (receiver, task)
}

/// Capture the active Tokio runtime without using a panic-based spawn helper.
fn runtime_handle() -> Result<Handle, ServerError> {
  Handle::try_current().map_err(|source| ServerError::RuntimeUnavailable {
    source,
  })
}

/// Read and deserialize one framed JSON-RPC message.
#[allow(
  clippy::single_call_fn,
  reason = "wire parsing isolates header validation, bounded body allocation, and protocol-error recovery"
)]
async fn read_message<R: AsyncBufRead + Unpin>(input: &mut R) -> Result<Option<InputEvent>, ServerError> {
  let mut content_length = None;
  let mut header = String::new();
  let mut saw_header = false;

  loop {
    header.clear();
    if input.read_line(&mut header).await? == 0 {
      return if saw_header {
        Err(ServerError::IncompleteHeader)
      } else {
        Ok(None)
      };
    }
    let Some(header_line) = header.strip_suffix("\r\n") else {
      return Err(ServerError::MalformedHeader {
        header: header.trim_end_matches('\n').to_owned(),
      });
    };
    if header_line.is_empty() {
      break;
    }
    saw_header = true;
    let Some((name, header_value)) = header_line.split_once(": ") else {
      return Err(ServerError::MalformedHeader {
        header: header_line.to_owned(),
      });
    };
    if name.eq_ignore_ascii_case("Content-Length") {
      let length = header_value
        .parse::<usize>()
        .map_err(|source| ServerError::InvalidContentLength {
          header_value: header_value.to_owned(),
          source,
        })?;
      if let Some(first) = content_length {
        return Err(ServerError::DuplicateContentLength {
          first,
          second: length,
        });
      }
      content_length = Some(length);
    }
  }

  let length = content_length.ok_or(ServerError::MissingContentLength)?;
  if length > MAXIMUM_MESSAGE_LENGTH {
    return Err(ServerError::MessageTooLarge {
      length,
      maximum: MAXIMUM_MESSAGE_LENGTH,
    });
  }
  let mut body = Vec::new();
  body
    .try_reserve_exact(length)
    .map_err(|source| ServerError::MessageAllocation {
      length,
      source,
    })?;
  body.resize(length, 0);
  let bytes_read = input.read_exact(&mut body).await?;
  tracing::trace!(bytes_read, "read complete LSP message body");
  Ok(Some(match rpc::decode_slice(&body) {
    Ok(message) => InputEvent::Message(message),
    Err(error) => InputEvent::ProtocolError(error.into_response()?),
  }))
}

#[cfg(test)]
mod tests {
  #[cfg(feature = "tokio-tcp")]
  use std::convert::Infallible;
  #[cfg(feature = "tokio-stdio")]
  use std::future::Ready;
  use std::future::pending;
  use std::future::poll_fn;
  use std::future::ready;
  use std::io;
  use std::io::ErrorKind;
  #[cfg(feature = "tokio-tcp")]
  use std::net;
  use std::pin::Pin;
  use std::task::Context;
  use std::task::Poll;

  use futures::SinkExt as _;
  use futures::Stream;
  use futures::StreamExt as _;
  use futures::channel::mpsc::TrySendError;
  use futures::channel::mpsc::unbounded;
  use futures::stream;
  use futures::task::noop_waker;
  use lsp_types::NumberOrString;
  use lsp_types::notification;
  use lsp_types::notification::Notification as _;
  #[cfg(feature = "tokio-stdio")]
  use lsp_types::request;
  use strict_test_support::PredicateFailure;
  use strict_test_support::ensure_that;
  #[cfg(feature = "tokio-stdio")]
  use tokio::io as test_io;
  use tokio::io::AsyncRead;
  use tokio::io::AsyncReadExt as _;
  use tokio::io::BufReader;
  use tokio::io::ReadBuf;
  use tokio::io::duplex;
  use tokio::runtime::Handle;
  #[cfg(feature = "tokio-stdio")]
  use tokio::task::JoinError;

  use super::InputEvent;
  use super::MAXIMUM_MESSAGE_LENGTH;
  use super::NativeTransport;
  use super::ShutdownSignalState;
  use super::TaskCompletion;
  use super::TransportTask;
  #[cfg(feature = "tokio-tcp")]
  use super::accept_or_shutdown;
  #[cfg(feature = "tokio-stdio")]
  use super::create_input;
  use super::is_exit_notification;
  use super::joined_task_result;
  use super::message_sink;
  use super::poll_transport_task;
  use super::preserve_primary;
  use super::read_message;
  use super::runtime_handle;
  use super::termination_result;
  use super::write_message;
  #[cfg(feature = "tokio-stdio")]
  use crate::ConcurrentContext;
  use crate::ConcurrentServer;
  use crate::MessageWriterError;
  #[cfg(feature = "tokio-stdio")]
  use crate::Params;
  use crate::ServerError;
  use crate::rpc;

  #[cfg(feature = "tokio-stdio")]
  /// Raw standard-I/O fixture capabilities kept separate from the mixed transport tests.
  mod stdio_fixture {
    use std::io;

    use tokio::io::AsyncWrite;
    use tokio::io::AsyncWriteExt as _;

    /// Write and flush one raw native-transport fixture frame.
    pub(super) async fn write_frame(output: &mut (impl AsyncWrite + Unpin), frame: &[u8]) -> io::Result<()> {
      output.write_all(frame).await?;
      output.flush().await
    }
  }

  #[cfg(feature = "tokio-stdio")]
  /// Initialize request used by the native transport lifecycle fixture.
  enum TransportInitialize {}

  #[cfg(feature = "tokio-stdio")]
  impl request::Request for TransportInitialize {
    type Params = ();
    type Result = ();
    const METHOD: &'static str = "initialize";
  }

  #[cfg(feature = "tokio-stdio")]
  /// Notification whose typed handler deliberately rejects transport work.
  enum RejectTransportWork {}

  #[cfg(feature = "tokio-stdio")]
  impl notification::Notification for RejectTransportWork {
    type Params = ();
    const METHOD: &'static str = "fixture/rejectTransportWork";
  }

  #[cfg(feature = "tokio-stdio")]
  /// Complete the transport initialize request successfully.
  fn initialize_transport(_context: ConcurrentContext<()>, _params: Params<()>) -> Ready<Result<(), rpc::RpcError>> {
    ready(Ok(()))
  }

  #[cfg(feature = "tokio-stdio")]
  /// Reject one independently executing transport notification.
  #[allow(
    clippy::single_call_fn,
    reason = "the rejecting handler fixture names the failing-notification contract exercised by the handler-failure test"
  )]
  fn reject_transport_work(_context: ConcurrentContext<()>, _params: Params<()>) -> Ready<Result<(), ServerError>> {
    ready(Err(ServerError::ExitBeforeShutdown))
  }

  #[cfg(feature = "tokio-stdio")]
  /// Construct one concrete-ID request for a native transport fixture.
  fn transport_request(method: &str, id: i32) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(method.into()),
      id: rpc::MessageId::Value(NumberOrString::Number(id)),
      ..rpc::Message::default()
    }
  }

  #[cfg(feature = "tokio-stdio")]
  /// Construct one notification for a native transport fixture.
  fn transport_notification(method: &str) -> rpc::Message {
    rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(method.into()),
      ..rpc::Message::default()
    }
  }

  /// Spawn a task whose cancellation is owned by fixture teardown.
  fn pending_transport_task() -> TransportTask {
    tokio::spawn(pending::<Result<(), ServerError>>())
  }

  /// Native fixture setup failures retaining runtime or channel sources.
  #[derive(Debug, thiserror::Error)]
  enum TransportFixtureFailure {
    /// Active runtime discovery failed.
    #[error(transparent)]
    Runtime(#[from] ServerError),
    /// The fixture input channel rejected its complete native event.
    #[error(transparent)]
    Enqueue(#[from] Box<TrySendError<InputEvent>>),
  }

  /// Reader that exposes one exact underlying I/O failure.
  struct FailingReader {
    /// Whether the configured failure has already been returned.
    failed: bool,
  }

  impl AsyncRead for FailingReader {
    fn poll_read(mut self: Pin<&mut Self>, _context: &mut Context<'_>, _buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
      if self.failed {
        Poll::Ready(Ok(()))
      } else {
        self.failed = true;
        Poll::Ready(Err(io::Error::from(ErrorKind::ConnectionReset)))
      }
    }
  }

  /// Native arbitration result, residual control state, and both teardown completions.
  #[derive(Debug)]
  struct ArbitrationObservations {
    /// Original input decision including native framing or task failures.
    input:    Result<Option<InputEvent>, ServerError>,
    /// Control stream state after the decision.
    shutdown: ShutdownSignalState,
    /// Both deliberately cancelled task completions.
    cleanup:  [Result<(), ServerError>; 2],
  }

  /// Observe one arbitration decision and finish both owned task cancellations.
  async fn arbitrate_ready_input<S>(shutdown_signals: S) -> Result<ArbitrationObservations, TransportFixtureFailure>
  where
    S: Stream<Item = ()> + Unpin,
  {
    let runtime = runtime_handle()?;
    let (sender, queued_input) = unbounded();
    sender
      .unbounded_send(InputEvent::Message(rpc::Message {
        jsonrpc: "2.0".into(),
        method: Some("initialized".into()),
        ..rpc::Message::default()
      }))
      .map_err(Box::new)?;
    let (output, _receiver) = unbounded();
    let mut transport = NativeTransport::new(
      runtime,
      (queued_input, pending_transport_task()),
      (output, pending_transport_task()),
      shutdown_signals,
      ShutdownSignalState::Open,
    );
    let input = transport.next_input().await;
    let shutdown = transport.shutdown_state;
    transport.input_task.abort();
    transport.output_task.abort();
    let cleanup = [
      joined_task_result("input", (&mut transport.input_task).await, true),
      joined_task_result("output", (&mut transport.output_task).await, true),
    ];
    Ok(ArbitrationObservations {
      input,
      shutdown,
      cleanup,
    })
  }

  /// Native request and notification subjects for terminal-input classification.
  type ExitMessages = [rpc::Message; 2];

  /// Complete arbitration setup or execution result.
  type ArbitrationOutcome = Result<ArbitrationObservations, TransportFixtureFailure>;

  /// Complete handler-initialization and failed notification effects.
  #[cfg(feature = "tokio-stdio")]
  type FailedHandler = (Result<(), ServerError>, FramingOutcome, Result<(), ServerError>, JoinedTransport);

  /// Native server task outcomes across completion and cancellation categories.
  type HandlerOutcomes = [Result<(), ServerError>; 4];

  /// Native transport lifecycle outcomes across all termination or cleanup cases.
  type LifecycleOutcomes = [Result<(), ServerError>; 3];

  #[test]
  fn only_the_exit_notification_terminates_protocol_input() -> Result<(), Box<PredicateFailure<ExitMessages>>> {
    let notification = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(notification::Exit::METHOD.into()),
      ..rpc::Message::default()
    };
    let request = rpc::Message {
      id: rpc::MessageId::Value(NumberOrString::Number(1)),
      ..notification.clone()
    };
    ensure_that(
      [notification, request],
      "only the exact exit notification may terminate protocol input",
      |messages| {
        let [ref observed_notification, ref observed_request] = *messages;
        is_exit_notification(observed_notification) && !is_exit_notification(observed_request)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Decode an entire in-memory transport fixture without erasing either native layer.
  async fn decode(input: &[u8]) -> Result<Option<InputEvent>, ServerError> {
    read_message(&mut BufReader::new(input)).await
  }

  /// Native framing completion including clean EOF and directional protocol events.
  type FramingOutcome = Result<Option<InputEvent>, ServerError>;

  #[tokio::test]
  async fn framing_accepts_one_content_length_and_decodes_the_message() -> Result<(), Box<PredicateFailure<FramingOutcome>>> {
    let body = br#"{"jsonrpc":"2.0","method":"initialized"}"#;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    ensure_that(
      decode(&frame).await,
      "one complete frame must retain its decoded notification",
      |outcome| matches!(*outcome, Ok(Some(InputEvent::Message(ref message))) if message.method.as_deref() == Some("initialized")),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[tokio::test]
  async fn invalid_json_becomes_a_null_id_protocol_error_response() -> Result<(), Box<PredicateFailure<FramingOutcome>>> {
    ensure_that(decode(b"Content-Length: 1\r\n\r\n{").await, "invalid framed JSON must retain a null-ID parse response", |outcome| matches!(*outcome, Ok(Some(InputEvent::ProtocolError(ref message))) if message.id == rpc::MessageId::Null && message.error.as_ref().is_some_and(|error| error.code == -32700))).map(drop).map_err(Box::new)
  }

  /// Complete rejected frame results and the checked oversized fixture length.
  type InvalidFrames = ([FramingOutcome; 5], Option<usize>, Option<FramingOutcome>);

  #[tokio::test]
  async fn framing_rejects_invalid_content_length_declarations() -> Result<(), Box<PredicateFailure<InvalidFrames>>> {
    let outcomes = [
      decode(b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}").await,
      decode(b"Content-Type: application/vscode-jsonrpc\r\n\r\n").await,
      decode(b"Content-Length: invalid\r\n\r\n").await,
      decode(b"Content-Length:2\r\n\r\n{}").await,
      decode(b"Content-Length: 2\n\n{}").await,
    ];
    let length = MAXIMUM_MESSAGE_LENGTH.checked_add(1);
    let oversized = match length {
      Some(oversized_length) => Some(decode(format!("Content-Length: {oversized_length}\r\n\r\n").as_bytes()).await),
      None => None,
    };
    ensure_that((outcomes, length, oversized), "invalid framing must retain duplicate, missing, malformed, and oversized declarations", |observed| {
      matches!(observed.0, [Err(ServerError::DuplicateContentLength { first: 2, second: 3 }), Err(ServerError::MissingContentLength), Err(ServerError::InvalidContentLength { .. }), Err(ServerError::MalformedHeader { ref header }), Err(ServerError::MalformedHeader { header: ref line })] if header == "Content-Length:2" && line == "Content-Length: 2")
        && matches!(observed.2, Some(Err(ServerError::MessageTooLarge { length: declared_length, maximum: MAXIMUM_MESSAGE_LENGTH })) if Some(declared_length) == observed.1)
    }).map(drop).map_err(Box::new)
  }

  /// Native clean, incomplete, truncated, and failed-reader outcomes.
  type EndedFrames = [FramingOutcome; 4];

  #[tokio::test]
  async fn framing_distinguishes_clean_eof_from_an_incomplete_header() -> Result<(), Box<PredicateFailure<EndedFrames>>> {
    let mut failing = BufReader::new(FailingReader {
      failed: false
    });
    ensure_that([decode(b"").await, decode(b"Content-Length: 2\r\n").await, decode(b"Content-Length: 3\r\n\r\n{}").await, read_message(&mut failing).await], "framing must distinguish clean EOF, incomplete headers, truncated bodies, and reader failures", |observed| {
      matches!(*observed, [Ok(None), Err(ServerError::IncompleteHeader), Err(ServerError::Io(ref truncated)), Err(ServerError::Io(ref source))] if truncated.kind() == ErrorKind::UnexpectedEof && source.kind() == ErrorKind::ConnectionReset)
    }).map(drop).map_err(Box::new)
  }

  /// Both channel send results and the complete delivered and expected message.
  type ChannelObservations = ([Result<(), MessageWriterError>; 2], Option<rpc::Message>, rpc::Message);

  #[tokio::test]
  async fn output_channel_preserves_messages_and_reports_broken_pipes() -> Result<(), Box<PredicateFailure<ChannelObservations>>> {
    let (sender, mut receiver) = unbounded();
    let mut sink = message_sink(sender);
    let expected = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some("initialized".into()),
      ..rpc::Message::default()
    };
    let sent = sink.send(expected.clone()).await;
    let delivered = receiver.next().await;
    drop(receiver);
    let rejected = sink.send(expected.clone()).await;
    ensure_that(
      ([sent, rejected], delivered, expected),
      "the output channel must preserve complete messages and its native disconnection failure",
      |observed| {
        let ([ref accepted, ref rejection], ref captured, ref original) = *observed;
        accepted.is_ok()
          && captured.as_ref() == Some(original)
          && matches!(*rejection, Err(ref error)
            if error.kind() == ErrorKind::BrokenPipe
              && matches!(*error, MessageWriterError::OutputChannelClosed { ref source } if source.is_disconnected()))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native framed write and read outcomes with all emitted bytes.
  type WrittenFrame = (Result<(), ServerError>, io::Result<usize>, Vec<u8>);

  #[tokio::test]
  async fn output_framing_serializes_one_complete_message() -> Result<(), Box<PredicateFailure<WrittenFrame>>> {
    let expected = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some("initialized".into()),
      ..rpc::Message::default()
    };
    let (mut reader, mut writer) = duplex(4_096);
    let written = write_message(&mut writer, expected).await;
    drop(writer);
    let mut framed = Vec::new();
    let read = reader.read_to_end(&mut framed).await;
    ensure_that(
      (written, read, framed),
      "output framing must emit its exact content length and complete JSON body",
      |observed| {
        observed.0.is_ok()
          && observed.1.as_ref().is_ok_and(|count| *count == observed.2.len())
          && observed.2 == b"Content-Length: 40\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"initialized\"}"
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native task join layers kept separate from server completion.
  #[cfg(feature = "tokio-stdio")]
  type JoinedTransport = Result<Result<(), ServerError>, JoinError>;

  #[cfg(feature = "tokio-stdio")]
  /// Input producer setup, framed delivery, and joined completion.
  type ClosedConsumer = Result<(Result<(), ServerError>, JoinedTransport), ServerError>;

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn input_producer_stops_cleanly_after_its_consumer_closes() -> Result<(), Box<PredicateFailure<ClosedConsumer>>> {
    let observed = match runtime_handle() {
      Ok(runtime) => {
        let (server_input, mut client_output) = duplex(4_096);
        let (input, task) = create_input(&runtime, server_input);
        drop(input);
        let written = write_message(&mut client_output, transport_notification("initialized")).await;
        drop(client_output);
        Ok((written, task.await))
      }
      Err(error) => Err(error),
    };
    ensure_that(
      observed,
      "input production must treat consumer closure as owned transport shutdown",
      |outcome| matches!(*outcome, Ok((Ok(()), Ok(Ok(()))))),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn runtime_discovery_reports_missing_context_without_string_erasure() -> Result<(), PredicateFailure<Result<Handle, ServerError>>> {
    ensure_that(
      runtime_handle(),
      "runtime discovery must retain Tokio's missing-context category",
      |outcome| matches!(*outcome, Err(ServerError::RuntimeUnavailable { ref source }) if source.is_missing_context()),
    )
    .map(drop)
  }

  #[tokio::test]
  async fn runtime_discovery_returns_the_active_handle() -> Result<(), PredicateFailure<Result<Handle, ServerError>>> {
    ensure_that(
      runtime_handle(),
      "runtime discovery inside Tokio must return its native handle",
      Result::is_ok,
    )
    .map(drop)
  }

  #[cfg(feature = "tokio-stdio")]
  /// One lifecycle request, its frame write, and complete native response event.
  #[derive(Debug)]
  struct LifecycleExchange {
    /// Expected response identifier retained with the operation.
    id:       i32,
    /// Framed client request delivery.
    write:    Result<(), ServerError>,
    /// Complete framed response.
    response: FramingOutcome,
  }

  #[cfg(feature = "tokio-stdio")]
  /// Complete lifecycle exchanges and terminal transport shutdown.
  type TransportLifecycle = (Vec<LifecycleExchange>, Result<(), ServerError>, JoinedTransport);

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_runs_the_complete_protocol_lifecycle() -> Result<(), Box<PredicateFailure<TransportLifecycle>>> {
    let (server_input, mut client_input) = duplex(4_096);
    let (client_output_stream, server_output) = duplex(4_096);
    let server = ConcurrentServer::new().on_request::<TransportInitialize, _>(initialize_transport);
    let transport = tokio::spawn(server.listen_stdio((), server_input, server_output, stream::empty()));
    let mut client_output = BufReader::new(client_output_stream);
    let mut exchanges = Vec::new();
    for (method, id) in [
      (<TransportInitialize as request::Request>::METHOD, 1),
      (<request::Shutdown as request::Request>::METHOD, 2),
    ] {
      let write = write_message(&mut client_input, transport_request(method, id)).await;
      let response = read_message(&mut client_output).await;
      exchanges.push(LifecycleExchange {
        id,
        write,
        response,
      });
    }
    let exit = write_message(&mut client_input, transport_notification(notification::Exit::METHOD)).await;
    let joined = transport.await;
    ensure_that((exchanges, exit, joined), "native transport must preserve each lifecycle response and complete initialization, shutdown, and exit", |observed| observed.0.len() == 2 && observed.0.iter().all(|exchange| exchange.write.is_ok() && matches!(exchange.response, Ok(Some(InputEvent::Message(ref message))) if message.id == rpc::MessageId::Value(NumberOrString::Number(exchange.id)) && message.result == Some(serde_json::Value::Null) && message.error.is_none())) && observed.1.is_ok() && matches!(observed.2, Ok(Ok(())))).map(drop).map_err(Box::new)
  }

  #[cfg(feature = "tokio-stdio")]
  /// Raw malformed-frame write, relayed response, external-stop send, and task completion.
  type RelayedError = (io::Result<()>, FramingOutcome, Result<(), TrySendError<()>>, JoinedTransport);

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_relays_protocol_errors_before_external_stop() -> Result<(), Box<PredicateFailure<RelayedError>>> {
    let (server_input, mut client_input) = duplex(4_096);
    let (client_output_stream, server_output) = duplex(4_096);
    let (shutdown_sender, shutdown_signals) = unbounded();
    let transport = tokio::spawn(ConcurrentServer::new().listen_stdio((), server_input, server_output, shutdown_signals));
    let mut client_output = BufReader::new(client_output_stream);
    let written = stdio_fixture::write_frame(&mut client_input, b"Content-Length: 1\r\n\r\n{").await;
    let response = read_message(&mut client_output).await;
    let stopped = shutdown_sender.unbounded_send(());
    let joined = transport.await;
    ensure_that((written, response, stopped, joined), "parse responses must relay before graceful external shutdown", |observed| observed.0.is_ok() && matches!(observed.1, Ok(Some(InputEvent::Message(ref message))) if message.id == rpc::MessageId::Null && message.error.as_ref().is_some_and(|error| error.code == -32700)) && observed.2.is_ok() && matches!(observed.3, Ok(Ok(())))).map(drop).map_err(Box::new)
  }

  #[cfg(feature = "tokio-stdio")]
  /// Reader, writer, and independently executing handler failure evidence.
  #[derive(Debug)]
  struct TransportFailures {
    /// Malformed-header delivery and transport completion.
    reader:  (io::Result<()>, JoinedTransport),
    /// Request delivery and closed response-stream failure.
    writer:  (Result<(), ServerError>, JoinedTransport),
    /// Initialization, its response, rejected notification, and transport completion.
    handler: FailedHandler,
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_preserves_reader_writer_and_handler_failures() -> Result<(), Box<PredicateFailure<TransportFailures>>> {
    let (reader_input, mut reader_client) = duplex(4_096);
    let (_reader_output, reader_sink) = duplex(4_096);
    let reader_task = tokio::spawn(ConcurrentServer::new().listen_stdio((), reader_input, reader_sink, stream::pending()));
    let reader_write = stdio_fixture::write_frame(&mut reader_client, b"Content-Type: application/vscode-jsonrpc\r\n\r\n").await;
    let reader = (reader_write, reader_task.await);

    let (writer_input, mut writer_client) = duplex(4_096);
    let (discarded, writer_sink) = duplex(4_096);
    drop(discarded);
    let writer_task = tokio::spawn(
      ConcurrentServer::new()
        .on_request::<TransportInitialize, _>(initialize_transport)
        .listen_stdio((), writer_input, writer_sink, stream::pending()),
    );
    let writer_write = write_message(
      &mut writer_client,
      transport_request(<TransportInitialize as request::Request>::METHOD, 3),
    )
    .await;
    let writer = (writer_write, writer_task.await);

    let (handler_input, mut handler_client) = duplex(4_096);
    let (handler_output, handler_sink) = duplex(4_096);
    let server = ConcurrentServer::new()
      .on_request::<TransportInitialize, _>(initialize_transport)
      .on_notification::<RejectTransportWork, _>(reject_transport_work);
    let handler_task = tokio::spawn(server.listen_stdio((), handler_input, handler_sink, stream::pending()));
    let mut output = BufReader::new(handler_output);
    let initialized = write_message(
      &mut handler_client,
      transport_request(<TransportInitialize as request::Request>::METHOD, 4),
    )
    .await;
    let response = read_message(&mut output).await;
    let notification = write_message(&mut handler_client, transport_notification(RejectTransportWork::METHOD)).await;
    let handler = (initialized, response, notification, handler_task.await);
    ensure_that(
      TransportFailures {
        reader,
        writer,
        handler,
      },
      "native transport must preserve distinct framing, writer I/O, and handler failures",
      |observed| {
        matches!(observed.reader, (Ok(()), Ok(Err(ServerError::MissingContentLength))))
          && matches!(observed.writer, (Ok(()), Ok(Err(ServerError::Io(ref source)))) if source.kind() == ErrorKind::BrokenPipe)
          && matches!(
            observed.handler,
            (
              Ok(()),
              Ok(Some(InputEvent::Message(_))),
              Ok(()),
              Ok(Err(ServerError::ExitBeforeShutdown))
            )
          )
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "tokio-stdio")]
  /// Idle external stop and terminal early-exit outcomes with complete output bytes.
  type EarlyExit = (
    Result<(), ServerError>,
    Result<(), ServerError>,
    JoinedTransport,
    io::Result<usize>,
    Vec<u8>,
  );

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_distinguishes_external_stop_from_early_exit() -> Result<(), Box<PredicateFailure<EarlyExit>>> {
    let stopped = ConcurrentServer::new()
      .listen_stdio((), test_io::empty(), test_io::sink(), stream::iter([()]))
      .await;
    let (server_input, mut client_input) = duplex(4_096);
    let (mut client_output, server_output) = duplex(4_096);
    let transport = tokio::spawn(ConcurrentServer::new().listen_stdio((), server_input, server_output, stream::pending()));
    let written = write_message(&mut client_input, transport_notification(notification::Exit::METHOD)).await;
    let joined = transport.await;
    let mut output = Vec::new();
    let read = client_output.read_to_end(&mut output).await;
    ensure_that(
      (stopped, written, joined, read, output),
      "external stop must succeed while early protocol exit retains its error and emits no response",
      |observed| {
        observed.0.is_ok()
          && observed.1.is_ok()
          && matches!(observed.2, Ok(Err(ServerError::ExitBeforeShutdown)))
          && matches!(observed.3, Ok(0))
          && observed.4.is_empty()
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[tokio::test]
  async fn external_shutdown_precedes_already_ready_protocol_input() -> Result<(), Box<PredicateFailure<ArbitrationOutcome>>> {
    ensure_that(
      arbitrate_ready_input(stream::iter([()])).await,
      "a ready external stop must win before protocol input without losing teardown outcomes",
      |outcome| {
        matches!(*outcome, Ok(ref observed)
          if matches!(observed.input, Ok(None))
            && observed.shutdown == ShutdownSignalState::Requested
            && observed.cleanup.iter().all(Result::is_ok))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[tokio::test]
  async fn closed_shutdown_stream_does_not_suppress_ready_protocol_input() -> Result<(), Box<PredicateFailure<ArbitrationOutcome>>> {
    ensure_that(
      arbitrate_ready_input(stream::empty()).await,
      "a closed external control stream must preserve ready protocol input and owned teardown",
      |outcome| {
        matches!(*outcome, Ok(ref observed)
          if matches!(observed.input, Ok(Some(InputEvent::Message(ref message))) if message.method.as_deref() == Some("initialized"))
            && observed.shutdown == ShutdownSignalState::Closed
            && observed.cleanup.iter().all(Result::is_ok))
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "tokio-tcp")]
  /// Complete TCP accept arbitration outcomes for all control-stream states.
  type TcpAcceptance = (
    [Result<Option<(u8, ShutdownSignalState)>, Infallible>; 3],
    io::Result<Option<(u8, ShutdownSignalState)>>,
  );

  #[cfg(feature = "tokio-tcp")]
  #[tokio::test]
  async fn tcp_acceptance_preserves_control_priority_and_residual_state() -> Result<(), Box<PredicateFailure<TcpAcceptance>>> {
    let accepted = [
      accept_or_shutdown(&mut stream::iter([()]), ready(Ok::<u8, Infallible>(1))).await,
      accept_or_shutdown(&mut stream::empty(), ready(Ok::<u8, Infallible>(2))).await,
      accept_or_shutdown(&mut stream::pending(), ready(Ok::<u8, Infallible>(3))).await,
    ];
    let failed = accept_or_shutdown(
      &mut stream::pending(),
      ready(Err::<u8, io::Error>(io::Error::from(ErrorKind::ConnectionAborted))),
    )
    .await;
    ensure_that(
      (accepted, failed),
      "TCP acceptance must preserve control priority, residual stream state, and native listener errors",
      |observed| {
        observed.0
          == [
            Ok(None),
            Ok(Some((2, ShutdownSignalState::Closed))),
            Ok(Some((3, ShutdownSignalState::Open))),
          ]
          && observed
            .1
            .as_ref()
            .is_err_and(|error| error.kind() == ErrorKind::ConnectionAborted)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[cfg(feature = "tokio-tcp")]
  /// External stop, reserved socket owner, address discovery, and bind rejection.
  #[derive(Debug)]
  struct TcpBinding {
    /// Idle listener's external stop result.
    stopped:  Result<(), ServerError>,
    /// Socket retained until the final bind assertion completes.
    listener: io::Result<net::TcpListener>,
    /// Native address lookup outcome.
    address:  Option<io::Result<net::SocketAddr>>,
    /// Native second bind result when setup provided an address.
    rejected: Option<Result<(), ServerError>>,
  }

  #[cfg(feature = "tokio-tcp")]
  #[tokio::test]
  async fn tcp_listener_distinguishes_external_stop_from_bind_failure() -> Result<(), Box<PredicateFailure<TcpBinding>>> {
    let ephemeral = net::SocketAddr::from(([127, 0, 0, 1], 0));
    let stopped = ConcurrentServer::new().listen_tcp((), ephemeral, stream::iter([()])).await;
    let listener = net::TcpListener::bind(ephemeral);
    let address = listener.as_ref().ok().map(net::TcpListener::local_addr);
    let rejected = match address.as_ref() {
      Some(&Ok(reserved_address)) => Some(
        ConcurrentServer::new()
          .listen_tcp((), reserved_address, stream::pending())
          .await,
      ),
      Some(&Err(_)) | None => None,
    };
    ensure_that(
      TcpBinding {
        stopped,
        listener,
        address,
        rejected,
      },
      "TCP listeners must distinguish graceful external stop from native address-in-use failure",
      |observed| {
        observed.stopped.is_ok()
          && observed.listener.is_ok()
          && matches!(observed.address, Some(Ok(_)))
          && matches!(observed.rejected, Some(Err(ServerError::Io(ref source))) if source.kind() == ErrorKind::AddrInUse)
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  /// Native successful/failed task polls and their committed completion states.
  type TaskPolls = (
    Result<(), ServerError>,
    TaskCompletion,
    Poll<Result<(), ServerError>>,
    Result<(), ServerError>,
    TaskCompletion,
  );

  #[tokio::test]
  async fn transport_task_polling_records_each_completion_once() -> Result<(), Box<PredicateFailure<TaskPolls>>> {
    let mut task = tokio::spawn(async { Ok(()) });
    let mut completion = TaskCompletion::Active;
    let first = poll_fn(|context| poll_transport_task("output", &mut task, &mut completion, context)).await;
    let waker = noop_waker();
    let repeated = poll_transport_task("output", &mut task, &mut completion, &mut Context::from_waker(&waker));
    let mut failed_task = tokio::spawn(async { Err(ServerError::MissingContentLength) });
    let mut failed_completion = TaskCompletion::Active;
    let failed = poll_fn(|context| poll_transport_task("input", &mut failed_task, &mut failed_completion, context)).await;
    ensure_that(
      (first, completion, repeated, failed, failed_completion),
      "each native task completion must commit once while preserving its original result",
      |observed| {
        matches!(
          *observed,
          (
            Ok(()),
            TaskCompletion::Complete,
            Poll::Pending,
            Err(ServerError::MissingContentLength),
            TaskCompletion::Complete
          )
        )
      },
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[tokio::test]
  async fn handler_join_distinguishes_intentional_transport_cancellation() -> Result<(), Box<PredicateFailure<HandlerOutcomes>>> {
    let completed = tokio::spawn(async { Ok(()) });
    let failed = tokio::spawn(async { Err(ServerError::ExitBeforeShutdown) });
    let intentional = tokio::spawn(pending::<Result<(), ServerError>>());
    intentional.abort();
    let unexpected = tokio::spawn(pending::<Result<(), ServerError>>());
    unexpected.abort();
    ensure_that([joined_task_result("handler", completed.await, false), joined_task_result("handler", failed.await, false), joined_task_result("handler", intentional.await, true), joined_task_result("handler", unexpected.await, false)], "handler joins must preserve completion, typed failure, and cancellation ownership", |observed| matches!(*observed, [Ok(()), Err(ServerError::ExitBeforeShutdown), Ok(()), Err(ServerError::TransportTaskTerminated { task: "handler", ref source })] if source.is_cancelled())).map(drop).map_err(Box::new)
  }

  #[test]
  fn transport_termination_distinguishes_protocol_exit_from_external_stop() -> Result<(), Box<PredicateFailure<LifecycleOutcomes>>> {
    ensure_that(
      [
        termination_result(true, false),
        termination_result(false, true),
        termination_result(false, false),
      ],
      "only completed protocol shutdown or an external process stop permits transport exit",
      |observed| matches!(*observed, [Ok(()), Ok(()), Err(ServerError::ExitBeforeShutdown)]),
    )
    .map(drop)
    .map_err(Box::new)
  }

  #[test]
  fn teardown_preserves_primary_failure_and_adopts_the_first_cleanup_failure() -> Result<(), Box<PredicateFailure<LifecycleOutcomes>>> {
    let mut established = Err(ServerError::ExitBeforeShutdown);
    preserve_primary(&mut established, Err(ServerError::MissingContentLength));
    let mut successful = Ok(());
    preserve_primary(&mut successful, Err(ServerError::MissingContentLength));
    let mut clean = Ok(());
    preserve_primary(&mut clean, Ok(()));
    ensure_that(
      [established, successful, clean],
      "teardown must preserve an established failure or adopt its first cleanup failure",
      |observed| {
        matches!(*observed, [
          Err(ServerError::ExitBeforeShutdown),
          Err(ServerError::MissingContentLength),
          Ok(())
        ])
      },
    )
    .map(drop)
    .map_err(Box::new)
  }
}
