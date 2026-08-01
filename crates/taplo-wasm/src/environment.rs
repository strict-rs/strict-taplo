//! Validated JavaScript host capabilities for local WebAssembly execution.

use std::any::type_name;
use std::fmt;
use std::io;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::task::Context;
use std::task::Poll;

use futures::FutureExt as _;
use js_sys::Date;
use js_sys::Function;
use js_sys::Promise;
use js_sys::Reflect;
use js_sys::Uint8Array;
use serde::de::DeserializeOwned;
use taplo_common::environment::Environment;
use taplo_common::environment::EnvironmentError;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::ReadBuf;
use url::Url;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_futures::spawn_local;

use crate::JsAsyncOperation;
use crate::JsAsyncRead;
use crate::JsAsyncWrite;
use crate::LocalWasmFuture;
use crate::WasmEnvironment;
use crate::js_error_message;

/// Describe a JavaScript value's runtime type.
fn js_type(javascript_value: &JsValue) -> String {
  javascript_value
    .js_typeof()
    .as_string()
    .unwrap_or_else(|| "unknown JavaScript value".into())
}

impl JsAsyncOperation {
  /// Construct shared pending state around one validated callback.
  const fn new(callback: Function) -> Self {
    Self {
      future: None,
      callback,
    }
  }

  /// Render only stable state without exposing the JavaScript callback.
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>, name: &'static str) -> fmt::Result {
    formatter
      .debug_struct(name)
      .field("pending", &self.future.is_some())
      .finish_non_exhaustive()
  }

  /// Start or resume one promise-returning callback operation.
  fn poll_callback(&mut self, context: &mut Context<'_>, argument: &JsValue) -> Poll<io::Result<JsValue>> {
    if self.future.is_none() {
      let returned = match self.callback.call1(&JsValue::null(), argument) {
        Ok(returned) => returned,
        Err(_javascript_error) => {
          return Poll::Ready(Err(io::Error::from(ErrorKind::BrokenPipe)));
        }
      };
      let Ok(promise) = returned.dyn_into::<Promise>() else {
        return Poll::Ready(Err(io::Error::from(ErrorKind::InvalidData)));
      };
      self.future = Some(JsFuture::from(promise));
    }

    let Some(future) = self.future.as_mut() else {
      return Poll::Ready(Err(io::Error::from(ErrorKind::InvalidData)));
    };
    match future.poll_unpin(context) {
      Poll::Ready(result) => {
        self.future = None;
        Poll::Ready(result.map_err(|_javascript_error| io::Error::from(ErrorKind::BrokenPipe)))
      }
      Poll::Pending => Poll::Pending,
    }
  }
}

impl fmt::Debug for JsAsyncRead {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.operation.fmt(formatter, "JsAsyncRead")
  }
}

impl JsAsyncRead {
  /// Construct a reader around a validated callback.
  fn new(callback: Function) -> Self {
    Self {
      operation: JsAsyncOperation::new(callback),
    }
  }
}

impl AsyncRead for JsAsyncRead {
  fn poll_read(mut self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
    let Poll::Ready(result) = self.operation.poll_callback(context, &JsValue::from(buffer.remaining())) else {
      return Poll::Pending;
    };
    Poll::Ready(result.and_then(|chunk| complete_read(chunk, buffer)))
  }
}

/// Decode one resolved standard-input callback result into the caller's buffer.
fn complete_read(chunk: JsValue, buffer: &mut ReadBuf<'_>) -> io::Result<()> {
  if !chunk.is_instance_of::<Uint8Array>() {
    return Err(io::Error::from(ErrorKind::InvalidData));
  }
  let bytes = Uint8Array::from(chunk).to_vec();
  if bytes.len() > buffer.remaining() {
    return Err(io::Error::from(ErrorKind::InvalidData));
  }
  buffer.put_slice(&bytes);
  Ok(())
}

impl fmt::Debug for JsAsyncWrite {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    self.operation.fmt(formatter, "JsAsyncWrite")
  }
}

impl JsAsyncWrite {
  /// Construct a writer around a validated callback.
  fn new(callback: Function) -> Self {
    Self {
      operation: JsAsyncOperation::new(callback),
    }
  }
}

impl AsyncWrite for JsAsyncWrite {
  fn poll_write(mut self: Pin<&mut Self>, context: &mut Context<'_>, buffer: &[u8]) -> Poll<Result<usize, io::Error>> {
    let argument = JsValue::from(Uint8Array::from(buffer));
    let Poll::Ready(result) = self.operation.poll_callback(context, &argument) else {
      return Poll::Pending;
    };
    Poll::Ready(result.and_then(|resolved| complete_write(resolved, buffer.len())))
  }

  fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
    Poll::Ready(Ok(()))
  }

  fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
    Poll::Ready(Ok(()))
  }
}

/// Decode one resolved writer callback result into an accepted byte count.
fn complete_write(resolved: JsValue, supplied: usize) -> io::Result<usize> {
  let written = match serde_wasm_bindgen::from_value::<usize>(resolved) {
    Ok(written) => written,
    Err(_decode_error) => return Err(io::Error::from(ErrorKind::InvalidData)),
  };
  if written > supplied {
    return Err(io::Error::from(ErrorKind::InvalidData));
  }
  Ok(written)
}

impl fmt::Debug for WasmEnvironment {
  fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    formatter.debug_struct("WasmEnvironment").finish_non_exhaustive()
  }
}

impl TryFrom<JsValue> for WasmEnvironment {
  type Error = EnvironmentError;

  fn try_from(host: JsValue) -> Result<Self, Self::Error> {
    Ok(Self {
      now:              callback(&host, "js_now")?,
      env_var:          callback(&host, "js_env_var")?,
      env_vars:         callback(&host, "js_env_vars")?,
      atty_stderr:      callback(&host, "js_atty_stderr")?,
      stdin:            callback(&host, "js_on_stdin")?,
      stdout:           callback(&host, "js_on_stdout")?,
      stderr:           callback(&host, "js_on_stderr")?,
      glob_files:       callback(&host, "js_glob_files")?,
      read_file:        callback(&host, "js_read_file")?,
      write_file:       callback(&host, "js_write_file")?,
      to_file_path:     callback(&host, "js_to_file_path")?,
      to_file_url:      callback(&host, "js_to_file_url")?,
      is_absolute:      callback(&host, "js_is_absolute")?,
      cwd:              callback(&host, "js_cwd")?,
      find_config_file: callback(&host, "js_find_config_file")?,
    })
  }
}

impl Environment for WasmEnvironment {
  type Stdin = JsAsyncRead;
  type Stdout = JsAsyncWrite;
  type Stderr = JsAsyncWrite;

  fn now(&self) -> Result<OffsetDateTime, EnvironmentError> {
    let returned = call0(&self.now, "js_now")?;
    let timestamp = if let Some(timestamp) = returned.as_string() {
      timestamp
    } else if returned.is_instance_of::<Date>() {
      date_to_timestamp(&returned)?
    } else {
      return Err(invalid_return("js_now", "Date or RFC 3339 string", &returned));
    };
    OffsetDateTime::parse(&timestamp, &Rfc3339).map_err(|error| EnvironmentError::InvalidTimestamp {
      name:    "js_now",
      message: error.to_string(),
    })
  }

  fn env_var(&self, name: &str) -> Result<Option<String>, EnvironmentError> {
    optional_string("js_env_var", &call1(&self.env_var, "js_env_var", &JsValue::from_str(name))?)
  }

  fn env_vars(&self) -> Result<Vec<(String, String)>, EnvironmentError> {
    deserialize_return("js_env_vars", call0(&self.env_vars, "js_env_vars")?)
  }

  fn atty_stderr(&self) -> Result<bool, EnvironmentError> {
    let returned = call0(&self.atty_stderr, "js_atty_stderr")?;
    returned
      .as_bool()
      .ok_or_else(|| invalid_return("js_atty_stderr", "boolean", &returned))
  }

  fn stdin(&self) -> Self::Stdin {
    JsAsyncRead::new(self.stdin.clone())
  }

  fn stdout(&self) -> Self::Stdout {
    JsAsyncWrite::new(self.stdout.clone())
  }

  fn stderr(&self) -> Self::Stderr {
    JsAsyncWrite::new(self.stderr.clone())
  }

  fn glob_files(&self, pattern: &str) -> Result<Vec<PathBuf>, EnvironmentError> {
    deserialize_return(
      "js_glob_files",
      call1(&self.glob_files, "js_glob_files", &JsValue::from_str(pattern))?,
    )
  }

  fn to_file_path(&self, url: &Url) -> Result<Option<PathBuf>, EnvironmentError> {
    optional_string(
      "js_to_file_path",
      &call1(&self.to_file_path, "js_to_file_path", &JsValue::from_str(url.as_str()))?,
    )
    .map(|path| path.map(Into::into))
  }

  fn to_file_url(&self, path: &Path) -> Result<Option<Url>, EnvironmentError> {
    let Some(input) = optional_string(
      "js_to_file_url",
      &call1(&self.to_file_url, "js_to_file_url", &JsValue::from_str(path_string(path)?))?,
    )?
    else {
      return Ok(None);
    };
    Url::parse(&input).map(Some).map_err(|source| EnvironmentError::InvalidUrl {
      name: "js_to_file_url",
      input,
      source,
    })
  }

  fn is_absolute(&self, path: &Path) -> Result<bool, EnvironmentError> {
    let returned = call1(&self.is_absolute, "js_is_absolute", &JsValue::from_str(path_string(path)?))?;
    returned
      .as_bool()
      .ok_or_else(|| invalid_return("js_is_absolute", "boolean", &returned))
  }

  fn cwd(&self) -> Result<Option<PathBuf>, EnvironmentError> {
    optional_string("js_cwd", &call0(&self.cwd, "js_cwd")?).map(|path| path.map(Into::into))
  }
}

taplo_common::implement_local_environment! {
  for WasmEnvironment {
    spawn |_environment, future| {
      spawn_local(future);
      Ok(())
    }
    read |environment, path| {
      let returned = call1(
        &environment.read_file,
        "js_read_file",
        &JsValue::from_str(path_string(path)?),
      )?;
      let resolved = await_promise("js_read_file", returned).await?;
      if !resolved.is_instance_of::<Uint8Array>() {
        return Err(invalid_return("js_read_file", "Promise<Uint8Array>", &resolved));
      }
      Ok(Uint8Array::from(resolved).to_vec())
    }
    write |environment, path, bytes| {
      let returned = environment
        .write_file
        .call2(
          &JsValue::null(),
          &JsValue::from_str(path_string(path)?),
          &JsValue::from(Uint8Array::from(bytes)),
        )
        .map_err(|error| EnvironmentError::Callback {
          name:    "js_write_file",
          message: js_error_message(&error),
      })?;
      let resolved = await_promise("js_write_file", returned).await?;
      deserialize_return("js_write_file", resolved)
    }
    find_config |environment, from| {
      optional_string(
        "js_find_config_file",
        &call1(
          &environment.find_config_file,
          "js_find_config_file",
          &JsValue::from_str(path_string(from)?),
        )?,
      )
      .map(|path| path.map(Into::into))
    }
  }
}

/// Borrow one path for the string-only JavaScript callback model.
fn path_string(path: &Path) -> Result<&str, EnvironmentError> {
  path.to_str().ok_or_else(|| EnvironmentError::InvalidPathUnicode {
    path: path.to_owned()
  })
}

/// Invoke `Date.prototype.toISOString` while retaining thrown exceptions as typed failures.
fn date_to_timestamp(javascript_value: &JsValue) -> Result<String, EnvironmentError> {
  let returned = Reflect::get(javascript_value, &JsValue::from_str("toISOString")).map_err(|error| EnvironmentError::Callback {
    name:    "js_now",
    message: js_error_message(&error),
  })?;
  let function = returned
    .dyn_ref::<Function>()
    .cloned()
    .ok_or_else(|| EnvironmentError::InvalidReturnType {
      name:     "js_now",
      expected: "Date with callable toISOString",
      actual:   "Date without callable toISOString".into(),
    })?;
  let timestamp = function.call0(javascript_value).map_err(|error| EnvironmentError::Callback {
    name:    "js_now",
    message: js_error_message(&error),
  })?;
  timestamp
    .as_string()
    .ok_or_else(|| invalid_return("js_now", "Date producing an RFC 3339 string", &timestamp))
}

/// Read and validate one required function property.
fn callback(host: &JsValue, name: &'static str) -> Result<Function, EnvironmentError> {
  let returned = Reflect::get(host, &JsValue::from_str(name)).map_err(|error| EnvironmentError::Callback {
    name,
    message: js_error_message(&error),
  })?;
  if returned.is_null() || returned.is_undefined() {
    return Err(EnvironmentError::MissingCallback {
      name,
    });
  }
  returned
    .dyn_ref::<Function>()
    .cloned()
    .ok_or(EnvironmentError::InvalidCallback {
      name,
    })
}

/// Invoke a zero-argument callback.
fn call0(callback: &Function, name: &'static str) -> Result<JsValue, EnvironmentError> {
  callback.call0(&JsValue::null()).map_err(|error| EnvironmentError::Callback {
    name,
    message: js_error_message(&error),
  })
}

/// Invoke a one-argument callback.
fn call1(callback: &Function, name: &'static str, argument: &JsValue) -> Result<JsValue, EnvironmentError> {
  callback
    .call1(&JsValue::null(), argument)
    .map_err(|error| EnvironmentError::Callback {
      name,
      message: js_error_message(&error),
    })
}

/// Await a value that must be a JavaScript promise.
fn await_promise(name: &'static str, returned: JsValue) -> LocalWasmFuture<'static, Result<JsValue, EnvironmentError>> {
  Box::pin(async move {
    let actual = js_type(&returned);
    let promise = returned
      .dyn_ref::<Promise>()
      .cloned()
      .ok_or(EnvironmentError::InvalidReturnType {
        name,
        expected: "Promise",
        actual,
      })?;
    JsFuture::from(promise).await.map_err(|error| EnvironmentError::Callback {
      name,
      message: js_error_message(&error),
    })
  })
}

/// Deserialize a callback result into one expected Rust value.
fn deserialize_return<T: DeserializeOwned>(name: &'static str, returned: JsValue) -> Result<T, EnvironmentError> {
  let actual = js_type(&returned);
  serde_wasm_bindgen::from_value(returned).map_err(|decode_error| EnvironmentError::InvalidReturnType {
    name,
    expected: type_name::<T>(),
    actual: format!("{actual}: {decode_error}"),
  })
}

/// Decode an optional string callback result.
fn optional_string(name: &'static str, returned: &JsValue) -> Result<Option<String>, EnvironmentError> {
  if returned.is_null() || returned.is_undefined() {
    Ok(None)
  } else {
    returned
      .as_string()
      .map(Some)
      .ok_or_else(|| invalid_return(name, "string, null, or undefined", returned))
  }
}

/// Construct an invalid-return error.
fn invalid_return(name: &'static str, expected: &'static str, returned: &JsValue) -> EnvironmentError {
  EnvironmentError::InvalidReturnType {
    name,
    expected,
    actual: js_type(returned),
  }
}
