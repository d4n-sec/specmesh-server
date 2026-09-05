use crate::ServerOptions;
use async_stream::stream;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::extract::State;
use axum::extract::connect_info::{ConnectInfo, Connected};
use axum::http::header::{
    ACCEPT, AUTHORIZATION, CACHE_CONTROL, CONTENT_ENCODING, CONTENT_TYPE, HOST, ORIGIN,
    WWW_AUTHENTICATE,
};
use axum::http::{HeaderMap, HeaderValue, Method, Request, Response, StatusCode};
use axum::response::IntoResponse;
use axum::routing::any;
use axum::serve::{IncomingStream, Listener};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use serde_json::{Value, json};
use specmesh_engine::engine::{ActionCancellation, ProtectedFileIdentity};
use specmesh_engine::mcp::{
    McpService, RoutedMessage, ToolRequest, jsonrpc_error, progress_notification, progress_phase,
    route_message, tool_response,
};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::fs::{File, OpenOptions};
use std::io::{self, IoSlice, Read};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::Path;
use std::pin::Pin;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;
use subtle::{Choice, ConstantTimeEq};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch};

const LOOPBACK: Ipv4Addr = Ipv4Addr::LOCALHOST;
const MCP_PATH: &str = "/mcp";
const TOKEN_TEXT_LEN: usize = 43;
const TOKEN_DECODED_LEN: usize = 32;
const PROGRESS_THRESHOLD: Duration = Duration::from_secs(2);
const MAX_IN_FLIGHT: usize = 32;

pub(crate) fn run(options: ServerOptions) -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            startup_error(&format!("cannot start HTTP runtime: {error}"));
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(serve(options)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            startup_error(&error);
            ExitCode::from(1)
        }
    }
}

async fn serve(options: ServerOptions) -> Result<(), String> {
    let port = options.port;
    let token_path = options.token_file.as_path();

    // Bind the process Workspace before accepting calls, but do not require it
    // to be initialized; init and doctor remain available on an empty root.
    let preliminary =
        McpService::bind(options.workspace.as_deref(), None).map_err(|error| error.to_string())?;
    let token = TokenFile::read(token_path, preliminary.workspace_root())?;
    let service = McpService::bind(
        Some(preliminary.workspace_root()),
        Some(token.identity.clone()),
    )
    .map_err(|error| error.to_string())?;

    // The address is intentionally not configurable. A requested port that
    // cannot be bound is an error; there is no port-0 or fallback behavior.
    let address = SocketAddr::V4(SocketAddrV4::new(LOOPBACK, port));
    let listener = tokio::net::TcpListener::bind(address)
        .await
        .map_err(|error| format!("cannot bind http://{address}{MCP_PATH}: {error}"))?;
    let listener = TrackedListener { listener };

    let state = HttpState::new(service, port, token.token);
    let app = Router::new()
        .route(MCP_PATH, any(mcp_endpoint))
        .fallback(not_found)
        .with_state(state.clone());
    let shutdown_state = state.clone();
    let shutdown = async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => shutdown_state.begin_shutdown(),
            Err(_) => shutdown_state.begin_shutdown(),
        }
    };

    let server_result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<ConnectionSignal>(),
    )
    .with_graceful_shutdown(shutdown)
    .await;
    state.begin_shutdown();
    state.wait_for_idle().await;
    server_result.map_err(|error| format!("HTTP MCP server failed: {error}"))
}

async fn not_found() -> Response<Body> {
    empty_response(StatusCode::NOT_FOUND)
}

async fn mcp_endpoint(
    State(state): State<HttpState>,
    ConnectInfo(connection): ConnectInfo<ConnectionSignal>,
    request: Request<Body>,
) -> Response<Body> {
    if let Some(rejection) = validate_request_boundary(&state, &request) {
        return rejection;
    }
    if request.method() != Method::POST {
        let mut response = empty_response(StatusCode::METHOD_NOT_ALLOWED);
        response
            .headers_mut()
            .insert("allow", HeaderValue::from_static("POST"));
        return response;
    }
    if request.headers().contains_key(ACCEPT) && !accepts_mcp_response(request.headers()) {
        return empty_response(StatusCode::NOT_ACCEPTABLE);
    }
    if state.shutting_down.load(Ordering::Acquire) {
        return empty_response(StatusCode::SERVICE_UNAVAILABLE);
    }

    let (parts, body) = request.into_parts();
    if !is_json_content_type(&parts.headers) {
        return empty_response(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    if has_unsupported_content_encoding(&parts.headers) {
        return empty_response(StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }
    let body = match tokio::select! {
        // The domain contract has no transport-specific transaction-size
        // limit. stdio and CLI likewise read the complete transaction, so the
        // authenticated loopback adapter must not reject an otherwise valid
        // request solely because it crosses an undocumented HTTP threshold.
        result = to_bytes(body, usize::MAX) => result,
        () = state.wait_for_shutdown() => return empty_response(StatusCode::SERVICE_UNAVAILABLE),
    } {
        Ok(body) => body,
        Err(_) => return empty_response(StatusCode::BAD_REQUEST),
    };
    let message: Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(error) => {
            return json_response(
                StatusCode::BAD_REQUEST,
                jsonrpc_error(Value::Null, -32700, &error.to_string()),
            );
        }
    };

    match route_message(message) {
        RoutedMessage::Notification | RoutedMessage::Cancel { .. } => {
            empty_response(StatusCode::ACCEPTED)
        }
        RoutedMessage::Immediate(response) => json_response(StatusCode::OK, response),
        RoutedMessage::Tool(tool) => run_tool(state, connection, tool).await,
    }
}

async fn run_tool(
    state: HttpState,
    connection: ConnectionSignal,
    tool: ToolRequest,
) -> Response<Body> {
    let permit = match state.try_acquire_capacity(&tool.id) {
        Ok(permit) => permit,
        Err(rejection) => return rejection.into_response(),
    };

    let internal_id = state.next_call_id.fetch_add(1, Ordering::Relaxed);
    let lifecycle = Arc::new(CallLifecycle::new());
    {
        let Ok(mut active) = state.active.lock() else {
            return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
        };
        active.insert(internal_id, lifecycle.clone());
    }
    if state.shutting_down.load(Ordering::Acquire) {
        lifecycle.cancelled.cancel();
    }
    let connection_lifecycle = lifecycle.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = connection.closed() => {
                if !connection_lifecycle.finished.load(Ordering::Acquire) {
                    connection_lifecycle.cancelled.cancel();
                }
            }
            () = connection_lifecycle.wait_until_completed() => {}
        }
    });

    let guard = DisconnectGuard {
        lifecycle: lifecycle.clone(),
    };
    let (sender, mut receiver) = oneshot::channel();
    let worker_state = state.clone();
    let worker_lifecycle = lifecycle.clone();
    let service = state.service.clone();
    let response_id = tool.id.clone();
    let tool_name = tool.name.clone();
    let arguments = tool.arguments;
    tokio::task::spawn_blocking(move || {
        let response = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            match service.call_tool(&tool_name, arguments, worker_lifecycle.cancelled.clone()) {
                Ok(action) => tool_response(response_id.clone(), action),
                Err(error) => jsonrpc_error(response_id.clone(), -32602, &error.0),
            }
        }))
        .unwrap_or_else(|_| jsonrpc_error(response_id, -32603, "tool execution panicked"));
        worker_lifecycle.cancelled.close_progress();
        worker_lifecycle.finished.store(true, Ordering::Release);
        worker_lifecycle.completed.notify_one();
        if let Ok(mut active) = worker_state.active.lock() {
            active.remove(&internal_id);
        }
        worker_state.idle.notify_one();
        drop(permit);
        let _ = sender.send(response);
    });

    let progress = tool.progress_token.zip(progress_phase(&tool.name));
    let Some(progress) = progress else {
        let response = receive_tool_response(&mut receiver, tool.id).await;
        drop(guard);
        return json_response(StatusCode::OK, response);
    };

    tokio::select! {
        biased;
        result = &mut receiver => {
            let response = result.unwrap_or_else(|_| {
                jsonrpc_error(tool.id, -32603, "tool execution ended without a response")
            });
            drop(guard);
            json_response(StatusCode::OK, response)
        }
        () = tokio::time::sleep(PROGRESS_THRESHOLD) => {
            if lifecycle.cancelled.is_stopped() {
                let response = receive_tool_response(&mut receiver, tool.id).await;
                drop(guard);
                json_response(StatusCode::OK, response)
            } else {
                sse_response(progress.0, progress.1, receiver, tool.id, guard)
            }
        }
    }
}

async fn receive_tool_response(receiver: &mut oneshot::Receiver<Value>, id: Value) -> Value {
    receiver
        .await
        .unwrap_or_else(|_| jsonrpc_error(id, -32603, "tool execution ended without a response"))
}

fn sse_response(
    progress_token: Value,
    phase: &'static str,
    receiver: oneshot::Receiver<Value>,
    id: Value,
    guard: DisconnectGuard,
) -> Response<Body> {
    let events = stream! {
        let _guard = guard;
        let mut receiver = receiver;
        let mut last = None;
        let mut interval = tokio::time::interval(PROGRESS_THRESHOLD);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                response = &mut receiver => {
                    let final_response = response.unwrap_or_else(|_| {
                        jsonrpc_error(id, -32603, "tool execution ended without a response")
                    });
                    yield Ok::<Bytes, Infallible>(sse_event(&final_response));
                    break;
                }
                _ = interval.tick() => {
                    if _guard.lifecycle.finished.load(Ordering::Acquire)
                        || _guard.lifecycle.cancelled.is_stopped()
                    {
                        continue;
                    }
                    let current = _guard.lifecycle.cancelled.progress_current();
                    if last.is_none_or(|previous| current > previous) {
                        if !_guard.lifecycle.cancelled.try_claim_progress() {
                            continue;
                        }
                        let progress = progress_notification(
                            progress_token.clone(),
                            phase,
                            current,
                        );
                        yield Ok::<Bytes, Infallible>(sse_event(&progress));
                        last = Some(current);
                    }
                }
            }
        }
    };
    let mut response = Response::new(Body::from_stream(events));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

fn sse_event(value: &Value) -> Bytes {
    Bytes::from(format!("event: message\ndata: {value}\n\n"))
}

fn validate_request_boundary(state: &HttpState, request: &Request<Body>) -> Option<Response<Body>> {
    let headers = request.headers();
    let expected_host = format!("127.0.0.1:{}", state.port);
    if single_header(headers, HOST).and_then(|value| value.to_str().ok())
        != Some(expected_host.as_str())
    {
        return Some(empty_response(StatusCode::FORBIDDEN));
    }
    if let Some(origins) = optional_single_header(headers, ORIGIN) {
        let expected_origin = format!("http://127.0.0.1:{}", state.port);
        if origins.and_then(|value| value.to_str().ok()) != Some(expected_origin.as_str()) {
            return Some(empty_response(StatusCode::FORBIDDEN));
        }
    }
    if !authorized(headers, &state.token) {
        let mut response = empty_response(StatusCode::UNAUTHORIZED);
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        return Some(response);
    }
    None
}

fn authorized(headers: &HeaderMap, expected: &[u8; TOKEN_TEXT_LEN]) -> bool {
    let authorization = single_header(headers, AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("Bearer"))
        .map(|(_, credential)| credential.as_bytes());

    let mut candidate = [0_u8; TOKEN_TEXT_LEN];
    let valid_length = authorization.is_some_and(|value| value.len() == TOKEN_TEXT_LEN);
    if let Some(value) = authorization {
        let length = value.len().min(TOKEN_TEXT_LEN);
        candidate[..length].copy_from_slice(&value[..length]);
    }
    let equal = expected.ct_eq(&candidate) & Choice::from(u8::from(valid_length));
    bool::from(equal)
}

fn single_header(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
) -> Option<&HeaderValue> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    Some(first)
}

fn optional_single_header(
    headers: &HeaderMap,
    name: axum::http::header::HeaderName,
) -> Option<Option<&HeaderValue>> {
    if !headers.contains_key(&name) {
        return None;
    }
    Some(single_header(headers, name))
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    single_header(headers, CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

fn has_unsupported_content_encoding(headers: &HeaderMap) -> bool {
    headers.get_all(CONTENT_ENCODING).iter().any(|value| {
        value
            .to_str()
            .map_or(true, |value| !value.eq_ignore_ascii_case("identity"))
    })
}

fn accepts_mcp_response(headers: &HeaderMap) -> bool {
    let mut accepts_json = false;
    let mut accepts_sse = false;
    for value in headers.get_all(ACCEPT) {
        let Ok(value) = value.to_str() else {
            return false;
        };
        for item in value.split(',') {
            let media_type = item.split(';').next().unwrap_or_default().trim();
            if media_type.eq_ignore_ascii_case("*/*") {
                return true;
            }
            accepts_json |= media_type.eq_ignore_ascii_case("application/json")
                || media_type.eq_ignore_ascii_case("application/*");
            accepts_sse |= media_type.eq_ignore_ascii_case("text/event-stream")
                || media_type.eq_ignore_ascii_case("text/*");
        }
    }
    accepts_json && accepts_sse
}

fn json_response(status: StatusCode, value: Value) -> Response<Body> {
    let mut response = axum::Json(value).into_response();
    *response.status_mut() = status;
    response
}

fn empty_response(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static HTTP response")
}

struct TrackedListener {
    listener: tokio::net::TcpListener,
}

impl Listener for TrackedListener {
    type Io = TrackedIo;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.listener.accept().await {
                Ok((stream, address)) => {
                    let signal = ConnectionSignal::new();
                    return (TrackedIo { stream, signal }, address);
                }
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

struct TrackedIo {
    stream: tokio::net::TcpStream,
    signal: ConnectionSignal,
}

impl AsyncRead for TrackedIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let remaining = buffer.remaining();
        let filled = buffer.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(context, buffer);
        match &result {
            Poll::Ready(Ok(())) if remaining > 0 && buffer.filled().len() == filled => {
                self.signal.mark_closed();
            }
            Poll::Ready(Err(_)) => self.signal.mark_closed(),
            _ => {}
        }
        result
    }
}

impl AsyncWrite for TrackedIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write(context, buffer);
        if matches!(result, Poll::Ready(Err(_))) {
            self.signal.mark_closed();
        }
        result
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffers: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let result = Pin::new(&mut self.stream).poll_write_vectored(context, buffers);
        if matches!(result, Poll::Ready(Err(_))) {
            self.signal.mark_closed();
        }
        result
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        let result = Pin::new(&mut self.stream).poll_flush(context);
        if matches!(result, Poll::Ready(Err(_))) {
            self.signal.mark_closed();
        }
        result
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

impl Drop for TrackedIo {
    fn drop(&mut self) {
        self.signal.mark_closed();
    }
}

#[derive(Clone)]
struct ConnectionSignal {
    state: Arc<ConnectionState>,
}

impl ConnectionSignal {
    fn new() -> Self {
        Self {
            state: Arc::new(ConnectionState {
                closed: AtomicBool::new(false),
                notify: Notify::new(),
            }),
        }
    }

    fn mark_closed(&self) {
        if !self.state.closed.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_one();
        }
    }

    async fn closed(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.state.closed.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for ConnectionSignal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConnectionSignal")
            .field("closed", &self.state.closed.load(Ordering::Relaxed))
            .finish()
    }
}

struct ConnectionState {
    closed: AtomicBool,
    notify: Notify,
}

impl Connected<IncomingStream<'_, TrackedListener>> for ConnectionSignal {
    fn connect_info(stream: IncomingStream<'_, TrackedListener>) -> Self {
        stream.io().signal.clone()
    }
}

#[derive(Clone)]
struct HttpState {
    service: McpService,
    port: u16,
    token: Arc<[u8; TOKEN_TEXT_LEN]>,
    capacity: Arc<Semaphore>,
    active: Arc<Mutex<BTreeMap<u64, Arc<CallLifecycle>>>>,
    next_call_id: Arc<AtomicU64>,
    shutting_down: Arc<AtomicBool>,
    shutdown: watch::Sender<bool>,
    idle: Arc<Notify>,
}

struct CapacityRejection {
    body: Value,
}

impl CapacityRejection {
    fn into_response(self) -> Response<Body> {
        json_response(StatusCode::OK, self.body)
    }
}

impl HttpState {
    fn new(service: McpService, port: u16, token: [u8; TOKEN_TEXT_LEN]) -> Self {
        let (shutdown, _) = watch::channel(false);
        Self {
            service,
            port,
            token: Arc::new(token),
            capacity: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
            active: Arc::new(Mutex::new(BTreeMap::new())),
            next_call_id: Arc::new(AtomicU64::new(1)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            shutdown,
            idle: Arc::new(Notify::new()),
        }
    }

    fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::Release);
        if let Ok(active) = self.active.lock() {
            for lifecycle in active.values() {
                lifecycle.cancelled.cancel();
            }
        }
        self.shutdown.send_replace(true);
        self.idle.notify_one();
    }

    fn try_acquire_capacity(
        &self,
        request_id: &Value,
    ) -> Result<OwnedSemaphorePermit, CapacityRejection> {
        self.capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| CapacityRejection {
                body: jsonrpc_error(
                    request_id.clone(),
                    -32000,
                    "too many MCP tool calls are active",
                ),
            })
    }

    async fn wait_for_shutdown(&self) {
        let mut shutdown = self.shutdown.subscribe();
        if *shutdown.borrow_and_update() {
            return;
        }
        let _ = shutdown.changed().await;
    }

    async fn wait_for_idle(&self) {
        loop {
            let empty = self
                .active
                .lock()
                .map(|active| active.is_empty())
                .unwrap_or(true);
            if empty {
                return;
            }
            self.idle.notified().await;
        }
    }
}

struct CallLifecycle {
    cancelled: Arc<ActionCancellation>,
    finished: AtomicBool,
    completed: Notify,
}

impl CallLifecycle {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(ActionCancellation::new()),
            finished: AtomicBool::new(false),
            completed: Notify::new(),
        }
    }

    async fn wait_until_completed(&self) {
        loop {
            let notified = self.completed.notified();
            if self.finished.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

struct DisconnectGuard {
    lifecycle: Arc<CallLifecycle>,
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if !self.lifecycle.finished.load(Ordering::Acquire) {
            self.lifecycle.cancelled.cancel();
        }
    }
}

struct TokenFile {
    token: [u8; TOKEN_TEXT_LEN],
    identity: ProtectedFileIdentity,
}

impl TokenFile {
    fn read(path: &Path, workspace: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err("--token-file must be an absolute path".into());
        }
        let path_metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot inspect --token-file: {error}"))?;
        if path_metadata.file_type().is_symlink() || !path_metadata.file_type().is_file() {
            return Err("--token-file must name a regular, non-symbolic-link file".into());
        }

        let mut file = open_token_file(path)?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect open --token-file: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("--token-file must remain a regular file while opening".into());
        }
        let opened_path_metadata = std::fs::symlink_metadata(path)
            .map_err(|error| format!("cannot recheck open --token-file: {error}"))?;
        if opened_path_metadata.file_type().is_symlink()
            || !opened_path_metadata.file_type().is_file()
            || !same_file_identity(&opened_path_metadata, &metadata)
        {
            return Err("--token-file identity changed while it was being opened".into());
        }
        let canonical_path = std::fs::canonicalize(path)
            .map_err(|error| format!("cannot normalize --token-file: {error}"))?;
        if canonical_path != path {
            return Err("--token-file must already be a normalized absolute path".into());
        }
        if canonical_path.starts_with(workspace) {
            return Err("--token-file must be outside the bound Workspace".into());
        }
        validate_token_permissions(&metadata)?;

        let mut contents = Vec::with_capacity(TOKEN_TEXT_LEN + 1);
        file.by_ref()
            .take((TOKEN_TEXT_LEN + 1) as u64)
            .read_to_end(&mut contents)
            .map_err(|error| format!("cannot read --token-file: {error}"))?;
        if contents.len() != TOKEN_TEXT_LEN || !contents.is_ascii() {
            return Err("--token-file must contain exactly 43 ASCII base64url characters".into());
        }
        let decoded = URL_SAFE_NO_PAD
            .decode(&contents)
            .map_err(|_| "--token-file content is not unpadded base64url".to_owned())?;
        if decoded.len() != TOKEN_DECODED_LEN {
            return Err("--token-file must encode exactly 32 bytes".into());
        }
        let token: [u8; TOKEN_TEXT_LEN] = contents
            .try_into()
            .expect("length was checked before token conversion");

        let identity = ProtectedFileIdentity::from_open_file(canonical_path, &metadata);

        Ok(Self { token, identity })
    }
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    true
}

fn open_token_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    options
        .open(path)
        .map_err(|error| format!("cannot open --token-file: {error}"))
}

#[cfg(unix)]
fn validate_token_permissions(metadata: &std::fs::Metadata) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    if mode & 0o400 == 0 || mode & 0o077 != 0 {
        return Err(
            "--token-file must be owner-readable and grant no group or other permissions".into(),
        );
    }
    Ok(())
}

#[cfg(not(unix))]
fn validate_token_permissions(_metadata: &std::fs::Metadata) -> Result<(), String> {
    Ok(())
}

fn startup_error(message: &str) {
    // Startup diagnostics intentionally include neither token bytes nor file
    // contents. The token is never serialized anywhere in the process.
    eprintln!("{}", json!({"type":"mcp_startup_error", "message":message}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_comparison_has_one_failure_shape() {
        let token = [b'A'; TOKEN_TEXT_LEN];
        let mut headers = HeaderMap::new();
        assert!(!authorized(&headers, &token));
        headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer wrong"));
        assert!(!authorized(&headers, &token));
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", "A".repeat(TOKEN_TEXT_LEN))).unwrap(),
        );
        assert!(authorized(&headers, &token));
    }

    #[test]
    fn content_type_is_strictly_json() {
        let mut headers = HeaderMap::new();
        assert!(!is_json_content_type(&headers));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
        assert!(!is_json_content_type(&headers));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/json; charset=utf-8"),
        );
        assert!(is_json_content_type(&headers));
    }

    #[tokio::test]
    async fn admission_is_exactly_32_and_overload_does_not_disturb_admitted_calls() {
        let workspace = tempfile::tempdir().unwrap();
        let service = McpService::bind(Some(workspace.path()), None).unwrap();
        let state = HttpState::new(service, 1, [b'A'; TOKEN_TEXT_LEN]);
        let mut admitted = Vec::new();
        for request_id in 1..=MAX_IN_FLIGHT {
            match state.try_acquire_capacity(&json!(request_id)) {
                Ok(permit) => admitted.push(permit),
                Err(_) => panic!("call {request_id} was rejected below the fixed capacity"),
            }
        }
        assert_eq!(admitted.len(), 32);
        assert_eq!(state.capacity.available_permits(), 0);

        let overload = match state.try_acquire_capacity(&json!(33)) {
            Ok(_) => panic!("the 33rd active call exceeded the fixed capacity"),
            Err(rejection) => rejection.into_response(),
        };
        assert_eq!(overload.status(), StatusCode::OK);
        assert!(
            overload.headers()[CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("application/json")
        );
        let body = to_bytes(overload.into_body(), usize::MAX).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({
                "jsonrpc":"2.0",
                "id":33,
                "error":{
                    "code":-32000,
                    "message":"too many MCP tool calls are active"
                }
            })
        );
        assert_eq!(state.capacity.available_permits(), 0);

        drop(admitted.pop());
        let replacement = match state.try_acquire_capacity(&json!(34)) {
            Ok(permit) => permit,
            Err(_) => panic!("one completed call did not admit one replacement"),
        };
        assert_eq!(state.capacity.available_permits(), 0);
        assert!(state.try_acquire_capacity(&json!(35)).is_err());
        drop(replacement);
        assert_eq!(state.capacity.available_permits(), 1);
        drop(admitted);
        assert_eq!(state.capacity.available_permits(), MAX_IN_FLIGHT);
    }
}
