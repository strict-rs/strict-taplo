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
  use futures::channel::mpsc::unbounded;
  use futures::stream;
  use futures::task::noop_waker;
  use lsp_types::NumberOrString;
  use lsp_types::notification;
  use lsp_types::notification::Notification as _;
  #[cfg(feature = "tokio-stdio")]
  use lsp_types::request;
  use strict_test_support::TestFailure;
  use strict_test_support::ensure;
  use strict_test_support::ensure_ok;
  use strict_test_support::ensure_some;
  #[cfg(feature = "tokio-stdio")]
  use tokio::io as test_io;
  use tokio::io::AsyncRead;
  use tokio::io::AsyncReadExt as _;
  use tokio::io::BufReader;
  use tokio::io::ReadBuf;
  use tokio::io::duplex;

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

  #[cfg(feature = "tokio-stdio")]
  /// Receive one complete JSON-RPC message from a framed client stream.
  async fn receive_transport_message(
    input: &mut BufReader<test_io::DuplexStream>,
    context: &'static str,
  ) -> Result<rpc::Message, TestFailure> {
    let event = ensure_some(ensure_ok(read_message(input).await, context)?, context)?;
    match event {
      InputEvent::Message(message) => Ok(message),
      InputEvent::ProtocolError(_) => ensure_some(None, "server output must contain valid JSON-RPC"),
    }
  }

  #[cfg(feature = "tokio-stdio")]
  /// Join one native transport driver without erasing either failure layer.
  async fn join_transport(task: TransportTask, context: &'static str) -> Result<Result<(), ServerError>, TestFailure> {
    ensure_ok(task.await, context)
  }

  /// Spawn one transport task that remains active until owned teardown.
  fn pending_transport_task() -> TransportTask {
    tokio::spawn(pending::<Result<(), ServerError>>())
  }

  /// Build a transport with input ready concurrently with the supplied control stream.
  #[allow(
    clippy::single_call_fn,
    reason = "the ready-input fixture centralizes the transport race state shared by the arbitration test"
  )]
  fn transport_with_ready_input<S>(shutdown_signals: S) -> Result<NativeTransport<S>, TestFailure>
  where
    S: Stream<Item = ()> + Unpin,
  {
    let runtime = ensure_ok(runtime_handle(), "the transport fixture requires the active runtime")?;
    let (input_sender, input) = unbounded();
    let input_message = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some("initialized".into()),
      ..rpc::Message::default()
    };
    ensure(
      input_sender.unbounded_send(InputEvent::Message(input_message)).is_ok(),
      "the transport fixture must enqueue its ready input event",
    )?;
    let (output, _output_receiver) = unbounded();
    Ok(NativeTransport::new(
      runtime,
      (input, pending_transport_task()),
      (output, pending_transport_task()),
      shutdown_signals,
      ShutdownSignalState::Open,
    ))
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

  /// Abort and join every pending fixture task under owned-teardown semantics.
  #[allow(
    clippy::single_call_fn,
    reason = "the fixture teardown helper verifies cancellation ownership for both pending transport tasks"
  )]
  async fn abort_transport<S>(transport: &mut NativeTransport<S>) -> Result<(), TestFailure> {
    transport.input_task.abort();
    transport.output_task.abort();
    ensure(
      joined_task_result("input", (&mut transport.input_task).await, true).is_ok(),
      "owned teardown must accept its input-task cancellation",
    )?;
    ensure(
      joined_task_result("output", (&mut transport.output_task).await, true).is_ok(),
      "owned teardown must accept its output-task cancellation",
    )
  }

  /// Observe one arbitration decision and clean up every pending fixture task.
  async fn arbitrate_ready_input<S>(
    shutdown_signals: S,
    context: &'static str,
  ) -> Result<(Option<InputEvent>, ShutdownSignalState), TestFailure>
  where
    S: Stream<Item = ()> + Unpin,
  {
    let mut transport = transport_with_ready_input(shutdown_signals)?;
    let input = ensure_ok(transport.next_input().await, context)?;
    let shutdown_state = transport.shutdown_state;
    abort_transport(&mut transport).await?;
    Ok((input, shutdown_state))
  }

  #[test]
  fn only_the_exit_notification_terminates_protocol_input() -> Result<(), TestFailure> {
    let exit_notification = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some(notification::Exit::METHOD.into()),
      ..rpc::Message::default()
    };
    ensure(
      is_exit_notification(&exit_notification),
      "an exact exit notification must terminate protocol input after lifecycle handling",
    )?;

    let exit_request = rpc::Message {
      id: rpc::MessageId::Value(NumberOrString::Number(1)),
      ..exit_notification
    };
    ensure(
      !is_exit_notification(&exit_request),
      "an exit request with an identifier must remain ordinary invalid protocol input",
    )
  }

  /// Decode one complete in-memory transport fixture.
  async fn decode(input: &[u8]) -> Result<Option<InputEvent>, ServerError> {
    read_message(&mut BufReader::new(input)).await
  }

  /// Require one in-memory transport fixture to fail framing.
  async fn decode_error(input: &[u8], context: &'static str) -> Result<ServerError, TestFailure> {
    ensure_some(decode(input).await.err(), context)
  }

  #[tokio::test]
  async fn framing_accepts_one_content_length_and_decodes_the_message() -> Result<(), TestFailure> {
    let body = br#"{"jsonrpc":"2.0","method":"initialized"}"#;
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend_from_slice(body);
    let event = ensure_some(
      decode(&frame).await.ok().flatten(),
      "a complete framed message must produce one input event",
    )?;
    match event {
      InputEvent::Message(message) => ensure(
        message.method.as_deref() == Some("initialized"),
        "the decoded message must preserve its method",
      ),
      InputEvent::ProtocolError(_) => ensure(false, "a valid JSON-RPC notification must not become a protocol-error response"),
    }
  }

  #[tokio::test]
  async fn invalid_json_becomes_a_null_id_protocol_error_response() -> Result<(), TestFailure> {
    let event = ensure_some(
      decode(b"Content-Length: 1\r\n\r\n{").await.ok().flatten(),
      "invalid framed JSON must produce one protocol event",
    )?;
    match event {
      InputEvent::ProtocolError(response) => {
        let error = ensure_some(response.error.as_ref(), "the protocol-error response must carry its JSON-RPC error")?;
        ensure(
          (&response.id, error.code) == (&rpc::MessageId::Null, -32700),
          "invalid JSON must produce the standard parse response with a JSON null ID",
        )
      }
      InputEvent::Message(_) => ensure(false, "invalid JSON must not be classified as a successful protocol message"),
    }
  }

  #[tokio::test]
  async fn framing_rejects_invalid_content_length_declarations() -> Result<(), TestFailure> {
    let error = decode_error(
      b"Content-Length: 2\r\nContent-Length: 3\r\n\r\n{}",
      "a repeated Content-Length header must fail",
    )
    .await?;
    ensure(
      matches!(error, ServerError::DuplicateContentLength {
        first: 2, second: 3
      }),
      "the duplicate header error must retain both declarations",
    )?;
    let missing = decode_error(
      b"Content-Type: application/vscode-jsonrpc\r\n\r\n",
      "a missing Content-Length header must fail",
    )
    .await?;
    ensure(
      matches!(missing, ServerError::MissingContentLength),
      "the missing header must retain its typed boundary",
    )?;

    let malformed = decode_error(b"Content-Length: invalid\r\n\r\n", "a malformed Content-Length header must fail").await?;
    ensure(
      matches!(malformed, ServerError::InvalidContentLength { .. }),
      "the malformed numeric value must retain its typed boundary",
    )?;
    let malformed_header = decode_error(
      b"Content-Length:2\r\n\r\n{}",
      "a header without the required field separator must fail",
    )
    .await?;
    ensure(
      matches!(
        malformed_header,
        ServerError::MalformedHeader {
          header
        } if header == "Content-Length:2"
      ),
      "a malformed header field must retain its exact source line",
    )?;
    let line_ending = decode_error(
      b"Content-Length: 2\n\n{}",
      "a header without the required CRLF line ending must fail",
    )
    .await?;
    ensure(
      matches!(
        line_ending,
        ServerError::MalformedHeader {
          header
        } if header == "Content-Length: 2"
      ),
      "a malformed header line ending must retain the exact offending line",
    )?;

    let oversized_length = ensure_some(
      MAXIMUM_MESSAGE_LENGTH.checked_add(1),
      "the framing limit must admit a larger test value",
    )?;
    let oversized_frame = format!("Content-Length: {oversized_length}\r\n\r\n");
    let oversized = decode_error(oversized_frame.as_bytes(), "an oversized message must fail before allocation").await?;
    ensure(
      matches!(
        oversized,
        ServerError::MessageTooLarge {
          length,
          maximum: MAXIMUM_MESSAGE_LENGTH
        } if length == oversized_length
      ),
      "the oversized message error must retain the declared length and transport limit",
    )
  }

  #[tokio::test]
  async fn framing_distinguishes_clean_eof_from_an_incomplete_header() -> Result<(), TestFailure> {
    ensure(
      decode(b"").await.ok().flatten().is_none(),
      "EOF before a header begins must close the input stream cleanly",
    )?;
    let incomplete = ensure_some(
      decode(b"Content-Length: 2\r\n").await.err(),
      "EOF after a header begins must fail the incomplete frame",
    )?;
    ensure(
      matches!(incomplete, ServerError::IncompleteHeader),
      "an incomplete header block must retain its typed transport boundary",
    )?;

    let truncated = decode_error(b"Content-Length: 3\r\n\r\n{}", "EOF within a declared message body must fail").await?;
    ensure(
      matches!(truncated, ServerError::Io(source) if source.kind() == ErrorKind::UnexpectedEof),
      "a truncated message body must retain the underlying unexpected-EOF category",
    )?;

    let mut failing = BufReader::new(FailingReader {
      failed: false
    });
    let read_failure = ensure_some(
      read_message(&mut failing).await.err(),
      "an underlying reader failure must reach the framing boundary",
    )?;
    ensure(
      matches!(read_failure, ServerError::Io(source) if source.kind() == ErrorKind::ConnectionReset),
      "an underlying reader failure must retain its exact I/O category",
    )
  }

  #[tokio::test]
  async fn output_channel_preserves_messages_and_reports_broken_pipes() -> Result<(), TestFailure> {
    let (sender, mut receiver) = unbounded();
    let mut sink = message_sink(sender);
    let expected = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some("initialized".into()),
      ..rpc::Message::default()
    };
    ensure_ok(
      sink.send(expected.clone()).await,
      "an open output channel must accept a complete JSON-RPC message",
    )?;
    let delivered = ensure_some(
      receiver.next().await,
      "the output receiver must observe the delivered JSON-RPC message",
    )?;
    ensure(
      delivered == expected,
      "the output channel must preserve the complete JSON-RPC message",
    )?;

    drop(receiver);
    let error = ensure_some(
      sink.send(expected).await.err(),
      "a closed output channel must reject message delivery",
    )?;
    ensure(
      error.kind() == ErrorKind::BrokenPipe,
      "a closed output channel must use the broken-pipe I/O category",
    )?;
    ensure(
      matches!(
        error,
        MessageWriterError::OutputChannelClosed {
          source
        } if source.is_disconnected()
      ),
      "the broken-pipe error must retain the futures disconnection source",
    )
  }

  #[tokio::test]
  async fn output_framing_serializes_one_complete_message() -> Result<(), TestFailure> {
    let expected = rpc::Message {
      jsonrpc: "2.0".into(),
      method: Some("initialized".into()),
      ..rpc::Message::default()
    };
    let (mut reader, mut writer) = duplex(4_096);
    ensure_ok(
      write_message(&mut writer, expected).await,
      "a writable transport must accept one JSON-RPC message",
    )?;
    drop(writer);
    let mut framed = Vec::new();
    let bytes_read = ensure_ok(
      reader.read_to_end(&mut framed).await,
      "the transport fixture must expose every framed byte",
    )?;
    ensure(
      (bytes_read, framed.as_slice())
        == (
          framed.len(),
          b"Content-Length: 40\r\n\r\n{\"jsonrpc\":\"2.0\",\"method\":\"initialized\"}",
        ),
      "output framing must emit an exact content-length header and JSON body",
    )
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn input_producer_stops_cleanly_after_its_consumer_closes() -> Result<(), TestFailure> {
    let runtime = ensure_ok(runtime_handle(), "the input-producer fixture requires the active runtime")?;
    let (server_input, mut client_output) = duplex(4_096);
    let (input, task) = create_input(&runtime, server_input);
    drop(input);

    ensure_ok(
      write_message(&mut client_output, transport_notification("initialized")).await,
      "the closed-consumer fixture must deliver one complete framed message",
    )?;
    drop(client_output);
    ensure_ok(
      join_transport(task, "the closed-consumer input producer must join").await?,
      "the input producer must treat loss of its consumer as owned transport shutdown",
    )
  }

  #[test]
  fn runtime_discovery_reports_missing_context_without_string_erasure() -> Result<(), TestFailure> {
    let error = ensure_some(runtime_handle().err(), "runtime discovery outside Tokio must fail")?;
    ensure(
      matches!(
        error,
        ServerError::RuntimeUnavailable {
          source
        } if source.is_missing_context()
      ),
      "runtime discovery must retain Tokio's missing-context category",
    )
  }

  #[tokio::test]
  async fn runtime_discovery_returns_the_active_handle() -> Result<(), TestFailure> {
    ensure(
      runtime_handle().is_ok(),
      "runtime discovery inside Tokio must return the active handle",
    )
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_runs_the_complete_protocol_lifecycle() -> Result<(), TestFailure> {
    let (server_input, mut client_input) = duplex(4_096);
    let (client_output_stream, server_output) = duplex(4_096);
    let server = ConcurrentServer::new().on_request::<TransportInitialize, _>(initialize_transport);
    let transport = tokio::spawn(server.listen_stdio((), server_input, server_output, stream::empty()));
    let mut client_output = BufReader::new(client_output_stream);

    for (method, request_id) in [
      (<TransportInitialize as request::Request>::METHOD, 1),
      (<request::Shutdown as request::Request>::METHOD, 2),
    ] {
      ensure_ok(
        write_message(&mut client_input, transport_request(method, request_id)).await,
        "the client must frame each lifecycle request",
      )?;
      let response = receive_transport_message(&mut client_output, "the transport must return each lifecycle response").await?;
      ensure(
        (response.id, response.result, response.error)
          == (
            rpc::MessageId::Value(NumberOrString::Number(request_id)),
            Some(serde_json::Value::Null),
            None,
          ),
        "the native transport must preserve each successful lifecycle response",
      )?;
    }

    ensure_ok(
      write_message(&mut client_input, transport_notification(notification::Exit::METHOD)).await,
      "the client must frame its terminal exit notification",
    )?;
    ensure_ok(
      join_transport(transport, "the native protocol transport task must join").await?,
      "initialize, shutdown, and exit must complete the native protocol transport",
    )
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_relays_protocol_errors_before_external_stop() -> Result<(), TestFailure> {
    let (server_input, mut client_input) = duplex(4_096);
    let (client_output_stream, server_output) = duplex(4_096);
    let (shutdown_sender, shutdown_signals) = unbounded();
    let transport = tokio::spawn(ConcurrentServer::new().listen_stdio((), server_input, server_output, shutdown_signals));
    let mut client_output = BufReader::new(client_output_stream);

    ensure_ok(
      stdio_fixture::write_frame(&mut client_input, b"Content-Length: 1\r\n\r\n{").await,
      "the malformed client frame must reach the native reader",
    )?;
    let response = receive_transport_message(&mut client_output, "the transport must relay a parse-error response").await?;
    let error = ensure_some(
      response.error.as_ref(),
      "the relayed parse-error response must carry a typed RPC error",
    )?;
    ensure(
      (&response.id, error.code) == (&rpc::MessageId::Null, -32700),
      "the native transport must relay the standard null-ID JSON parse error",
    )?;

    ensure(
      shutdown_sender.unbounded_send(()).is_ok(),
      "the external-stop fixture must remain connected",
    )?;
    ensure_ok(
      join_transport(transport, "the externally stopped protocol transport must join").await?,
      "external stop after a relayed protocol error must remain graceful",
    )
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_preserves_reader_writer_and_handler_failures() -> Result<(), TestFailure> {
    let (malformed_server_input, mut malformed_client_input) = duplex(4_096);
    let (_malformed_client_output, malformed_server_output) = duplex(4_096);
    let malformed_transport =
      tokio::spawn(ConcurrentServer::new().listen_stdio((), malformed_server_input, malformed_server_output, stream::pending()));
    ensure_ok(
      stdio_fixture::write_frame(&mut malformed_client_input, b"Content-Type: application/vscode-jsonrpc\r\n\r\n").await,
      "the malformed transport header must reach the native reader",
    )?;
    let malformed_result = join_transport(malformed_transport, "the malformed-header transport task must join").await?;
    ensure(
      matches!(malformed_result, Err(ServerError::MissingContentLength)),
      "the stdio listener must preserve the reader's typed framing failure",
    )?;

    let (writer_server_input, mut writer_client_input) = duplex(4_096);
    let (discarded_client_output, writer_server_output) = duplex(4_096);
    drop(discarded_client_output);
    let writer_transport = tokio::spawn(
      ConcurrentServer::new()
        .on_request::<TransportInitialize, _>(initialize_transport)
        .listen_stdio((), writer_server_input, writer_server_output, stream::pending()),
    );
    ensure_ok(
      write_message(
        &mut writer_client_input,
        transport_request(<TransportInitialize as request::Request>::METHOD, 3),
      )
      .await,
      "the client must deliver a request whose response stream is closed",
    )?;
    let writer_result = join_transport(writer_transport, "the failed-writer transport task must join").await?;
    ensure(
      matches!(
        writer_result,
        Err(ServerError::Io(source)) if source.kind() == ErrorKind::BrokenPipe
      ),
      "the stdio listener must preserve the writer's broken-pipe category",
    )?;

    let (handler_server_input, mut handler_client_input) = duplex(4_096);
    let (handler_client_output, handler_server_output) = duplex(4_096);
    let server = ConcurrentServer::new()
      .on_request::<TransportInitialize, _>(initialize_transport)
      .on_notification::<RejectTransportWork, _>(reject_transport_work);
    let handler_transport = tokio::spawn(server.listen_stdio((), handler_server_input, handler_server_output, stream::pending()));
    let mut handler_output = BufReader::new(handler_client_output);
    ensure_ok(
      write_message(
        &mut handler_client_input,
        transport_request(<TransportInitialize as request::Request>::METHOD, 4),
      )
      .await,
      "the handler-failure fixture must initialize its protocol session",
    )?;
    drop(
      receive_transport_message(
        &mut handler_output,
        "the handler-failure fixture must receive its initialize response",
      )
      .await?,
    );
    ensure_ok(
      write_message(&mut handler_client_input, transport_notification(RejectTransportWork::METHOD)).await,
      "the client must deliver the rejected independent notification",
    )?;
    let handler_result = join_transport(handler_transport, "the failed-handler transport task must join").await?;
    ensure(
      matches!(handler_result, Err(ServerError::ExitBeforeShutdown)),
      "the stdio listener must preserve the handler's typed server failure",
    )
  }

  #[cfg(feature = "tokio-stdio")]
  #[tokio::test]
  async fn stdio_transport_distinguishes_external_stop_from_early_exit() -> Result<(), TestFailure> {
    ensure_ok(
      ConcurrentServer::new()
        .listen_stdio((), test_io::empty(), test_io::sink(), stream::iter([()]))
        .await,
      "an external process stop must end an idle stdio transport gracefully",
    )?;

    let (server_input, mut client_input) = duplex(4_096);
    let (mut client_output, server_output) = duplex(4_096);
    let transport = tokio::spawn(ConcurrentServer::new().listen_stdio((), server_input, server_output, stream::pending()));
    ensure_ok(
      write_message(&mut client_input, transport_notification(notification::Exit::METHOD)).await,
      "the client must deliver its premature exit notification",
    )?;
    let result = join_transport(transport, "the premature-exit transport task must join").await?;
    ensure(
      matches!(result, Err(ServerError::ExitBeforeShutdown)),
      "protocol exit without shutdown must remain a typed lifecycle failure",
    )?;
    let mut output = Vec::new();
    let output_length = ensure_ok(
      client_output.read_to_end(&mut output).await,
      "the closed premature-exit transport must expose its complete output",
    )?;
    ensure(
      (output_length, output.is_empty()) == (0, true),
      "a terminal notification must not fabricate a JSON-RPC response",
    )
  }

  #[tokio::test]
  async fn external_shutdown_precedes_already_ready_protocol_input() -> Result<(), TestFailure> {
    let (input, shutdown_state) = arbitrate_ready_input(stream::iter([()]), "the prioritized transport decision must be available").await?;
    ensure(
      (input.is_some(), shutdown_state) == (false, ShutdownSignalState::Requested),
      "an external stop request must win when protocol input is ready concurrently",
    )
  }

  #[tokio::test]
  async fn closed_shutdown_stream_does_not_suppress_ready_protocol_input() -> Result<(), TestFailure> {
    let (input, shutdown_state) = arbitrate_ready_input(
      stream::empty(),
      "ready protocol input must remain available after the control stream closes",
    )
    .await?;
    let message = match input {
      Some(InputEvent::Message(message)) => message,
      Some(InputEvent::ProtocolError(_)) | None => {
        return ensure(false, "ready protocol input must remain a successful message");
      }
    };
    ensure(
      (message.method.as_deref(), shutdown_state) == (Some("initialized"), ShutdownSignalState::Closed),
      "closing the external stop stream must disable only that control source",
    )
  }

  #[cfg(feature = "tokio-tcp")]
  /// Accept one ready fixture transport while preserving the external-signal state.
  async fn accept_fixture<S>(signals: &mut S, value: u8) -> Result<(u8, ShutdownSignalState), TestFailure>
  where
    S: Stream<Item = ()> + Unpin,
  {
    ensure_some(
      ensure_ok(
        accept_or_shutdown(signals, ready(Ok::<u8, Infallible>(value))).await,
        "the ready TCP fixture must complete acceptance",
      )?,
      "a non-triggering stop stream must preserve the accepted value",
    )
  }

  #[cfg(feature = "tokio-tcp")]
  #[tokio::test]
  async fn tcp_acceptance_preserves_control_priority_and_residual_state() -> Result<(), TestFailure> {
    let mut requested = stream::iter([()]);
    let stopped = ensure_ok(
      accept_or_shutdown(&mut requested, ready(Ok::<u8, Infallible>(1))).await,
      "a ready external stop must resolve TCP acceptance",
    )?;
    ensure(
      stopped.is_none(),
      "a real external stop must win over a concurrently ready TCP acceptance",
    )?;

    let mut closed = stream::empty();
    let accepted_after_close = accept_fixture(&mut closed, 2).await?;
    ensure(
      accepted_after_close == (2, ShutdownSignalState::Closed),
      "a closed stop stream must remain closed when the accepted transport enters the driver",
    )?;

    let mut open = stream::pending();
    let accepted_while_open = accept_fixture(&mut open, 3).await?;
    ensure(
      accepted_while_open == (3, ShutdownSignalState::Open),
      "a pending stop stream must remain open when the accepted transport enters the driver",
    )?;

    let mut accept_error_shutdown = stream::pending();
    let accept_error = ensure_some(
      accept_or_shutdown(
        &mut accept_error_shutdown,
        ready(Err::<u8, io::Error>(io::Error::from(ErrorKind::ConnectionAborted))),
      )
      .await
      .err(),
      "a failed TCP acceptance must retain its typed transport error",
    )?;
    ensure(
      accept_error.kind() == ErrorKind::ConnectionAborted,
      "TCP accept arbitration must preserve the listener's I/O error category",
    )
  }

  #[cfg(feature = "tokio-tcp")]
  #[tokio::test]
  async fn tcp_listener_distinguishes_external_stop_from_bind_failure() -> Result<(), TestFailure> {
    let ephemeral_address = net::SocketAddr::from(([127, 0, 0, 1], 0));
    ensure_ok(
      ConcurrentServer::new()
        .listen_tcp((), ephemeral_address, stream::iter([()]))
        .await,
      "an external process stop must end an idle TCP listener gracefully",
    )?;

    let occupied = ensure_ok(
      net::TcpListener::bind(ephemeral_address),
      "the bind-failure fixture must reserve one local address",
    )?;
    let occupied_address = ensure_ok(occupied.local_addr(), "the bind-failure fixture must expose its reserved address")?;
    let error = ensure_some(
      ConcurrentServer::new()
        .listen_tcp((), occupied_address, stream::pending())
        .await
        .err(),
      "binding a second listener to the reserved address must fail",
    )?;
    ensure(
      matches!(error, ServerError::Io(source) if source.kind() == ErrorKind::AddrInUse),
      "the TCP listener must preserve the operating system's address-in-use category",
    )
  }

  #[tokio::test]
  async fn transport_task_polling_records_each_completion_once() -> Result<(), TestFailure> {
    let mut task_handle = tokio::spawn(async { Ok(()) });
    let mut completion = TaskCompletion::Active;
    ensure_ok(
      poll_fn(|context| poll_transport_task("output", &mut task_handle, &mut completion, context)).await,
      "an active successful transport task must expose its completion",
    )?;
    ensure(
      completion == TaskCompletion::Complete,
      "observing a transport task must commit its completion state",
    )?;

    let waker = noop_waker();
    let mut context = Context::from_waker(&waker);
    ensure(
      matches!(
        poll_transport_task("output", &mut task_handle, &mut completion, &mut context,),
        Poll::Pending
      ),
      "an already interpreted transport task must not be polled a second time",
    )?;

    let mut failed_task = tokio::spawn(async { Err(ServerError::MissingContentLength) });
    let mut failed_completion = TaskCompletion::Active;
    let error = ensure_some(
      poll_fn(|task_context| poll_transport_task("input", &mut failed_task, &mut failed_completion, task_context))
        .await
        .err(),
      "an active failed transport task must expose its typed result",
    )?;
    ensure(
      (matches!(error, ServerError::MissingContentLength), failed_completion) == (true, TaskCompletion::Complete),
      "polling a failed transport task must preserve its failure and commit completion exactly once",
    )
  }

  #[tokio::test]
  async fn handler_join_distinguishes_intentional_transport_cancellation() -> Result<(), TestFailure> {
    let completed = tokio::spawn(async { Ok(()) });
    ensure(
      joined_task_result("handler", completed.await, false).is_ok(),
      "a normally completed handler task must preserve its successful result",
    )?;

    let failed = tokio::spawn(async { Err(ServerError::ExitBeforeShutdown) });
    ensure(
      matches!(
        joined_task_result("handler", failed.await, false),
        Err(ServerError::ExitBeforeShutdown)
      ),
      "a handler's typed server failure must propagate without conversion",
    )?;

    let intentionally_cancelled = tokio::spawn(pending::<Result<(), ServerError>>());
    intentionally_cancelled.abort();
    ensure(
      joined_task_result("handler", intentionally_cancelled.await, true).is_ok(),
      "transport shutdown must accept the cancellation it deliberately issued",
    )?;

    let unexpectedly_cancelled = tokio::spawn(pending::<Result<(), ServerError>>());
    unexpectedly_cancelled.abort();
    let error = ensure_some(
      joined_task_result("handler", unexpectedly_cancelled.await, false).err(),
      "a handler cancelled while the transport remains live must be reported",
    )?;
    ensure(
      matches!(
        error,
        ServerError::TransportTaskTerminated {
          task: "handler",
          source
        } if source.is_cancelled()
      ),
      "unexpected task cancellation must retain its role and typed Tokio source",
    )
  }

  #[test]
  fn transport_termination_distinguishes_protocol_exit_from_external_stop() -> Result<(), TestFailure> {
    ensure(
      termination_result(true, false).is_ok(),
      "a completed protocol shutdown must permit transport exit",
    )?;
    ensure(
      termination_result(false, true).is_ok(),
      "an external process stop must not fabricate an LSP shutdown request",
    )?;
    ensure(
      matches!(termination_result(false, false), Err(ServerError::ExitBeforeShutdown)),
      "an ordinary client exit before shutdown must remain a lifecycle error",
    )
  }

  #[test]
  fn teardown_preserves_primary_failure_and_adopts_the_first_cleanup_failure() -> Result<(), TestFailure> {
    let mut established = Err(ServerError::ExitBeforeShutdown);
    preserve_primary(&mut established, Err(ServerError::MissingContentLength));
    ensure(
      matches!(established, Err(ServerError::ExitBeforeShutdown)),
      "transport teardown must not replace the established primary failure",
    )?;

    let mut successful = Ok(());
    preserve_primary(&mut successful, Err(ServerError::MissingContentLength));
    ensure(
      matches!(successful, Err(ServerError::MissingContentLength)),
      "transport teardown must adopt its first failure when primary work succeeded",
    )?;

    let mut clean = Ok(());
    preserve_primary(&mut clean, Ok(()));
    ensure(clean.is_ok(), "successful primary work and cleanup must remain successful")
  }
}
