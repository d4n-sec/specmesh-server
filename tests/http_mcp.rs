#![cfg(unix)]

use fs2::FileExt;
use reqwest::StatusCode;
use reqwest::blocking::{Client, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, ORIGIN, WWW_AUTHENTICATE};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Ipv4Addr, Shutdown, TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Barrier, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const TOKEN: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const ROTATED_TOKEN: &str = "__________________________________________8";
static LONG_TEST_LOCK: Mutex<()> = Mutex::new(());

struct HttpServer {
    child: Child,
    endpoint: String,
    port: u16,
}

impl HttpServer {
    fn start(workspace: &Path, token_file: &Path) -> Self {
        Self::spawn(Some(workspace), None, token_file)
    }

    fn start_in_current_directory(current_directory: &Path, token_file: &Path) -> Self {
        Self::spawn(None, Some(current_directory), token_file)
    }

    fn spawn(
        workspace: Option<&Path>,
        current_directory: Option<&Path>,
        token_file: &Path,
    ) -> Self {
        let port = unused_port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_specmesh-server"));
        if let Some(workspace) = workspace {
            command.args(["--workspace", workspace.to_str().unwrap()]);
        }
        command.args([
            "--port",
            &port.to_string(),
            "--token-file",
            token_file.to_str().unwrap(),
        ]);
        if let Some(current_directory) = current_directory {
            command.current_dir(current_directory);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            endpoint: format!("http://127.0.0.1:{port}/mcp"),
            port,
        };
        server.wait_until_ready();
        server
    }

    fn wait_until_ready(&mut self) {
        let client = client();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(response) = client
                .get(&self.endpoint)
                .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                .send()
                && response.status() == StatusCode::METHOD_NOT_ALLOWED
            {
                assert_loopback_listener(self.child.id(), self.port);
                return;
            }
            if let Some(status) = self.child.try_wait().unwrap() {
                let mut stdout = String::new();
                if let Some(mut pipe) = self.child.stdout.take() {
                    pipe.read_to_string(&mut stdout).unwrap();
                }
                let mut stderr = String::new();
                if let Some(mut pipe) = self.child.stderr.take() {
                    pipe.read_to_string(&mut stderr).unwrap();
                }
                panic!(
                    "HTTP MCP exited during startup with {status}: stdout={stdout}; stderr={stderr}"
                );
            }
            assert!(Instant::now() < deadline, "HTTP MCP did not become ready");
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn post(&self, message: &Value) -> Response {
        client()
            .post(&self.endpoint)
            .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("accept", "application/json, text/event-stream")
            .json(message)
            .send()
            .unwrap()
    }

    fn interrupt_and_wait(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            let status = Command::new("kill")
                .args(["-INT", &self.child.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.child.try_wait().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "HTTP MCP did not shut down after SIGINT"
            );
            thread::sleep(Duration::from_millis(25));
        }
        let mut stdout = String::new();
        if let Some(mut pipe) = self.child.stdout.take() {
            pipe.read_to_string(&mut stdout).unwrap();
        }
        let mut stderr = String::new();
        if let Some(mut pipe) = self.child.stderr.take() {
            pipe.read_to_string(&mut stderr).unwrap();
        }
        assert!(
            !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
            "HTTP MCP logged its bearer token"
        );
    }
}

#[cfg(target_os = "linux")]
fn assert_loopback_listener(process_id: u32, port: u16) {
    let port = format!("{port:04X}");
    let table = fs::read_to_string(format!("/proc/{process_id}/net/tcp")).unwrap();
    let listeners: Vec<_> = table
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_ascii_whitespace().collect();
            (fields.len() > 3 && fields[3] == "0A")
                .then_some(fields[1])
                .filter(|address| address.ends_with(&format!(":{port}")))
        })
        .collect();
    assert_eq!(listeners, [format!("0100007F:{port}")]);

    let table6 = fs::read_to_string(format!("/proc/{process_id}/net/tcp6")).unwrap();
    assert!(!table6.lines().any(|line| {
        let fields: Vec<_> = line.split_ascii_whitespace().collect();
        fields.len() > 3 && fields[3] == "0A" && fields[1].ends_with(&format!(":{port}"))
    }));
}

#[cfg(not(target_os = "linux"))]
fn assert_loopback_listener(_process_id: u32, _port: u16) {}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn client() -> Client {
    client_with_timeout(Duration::from_secs(30))
}

fn client_with_timeout(timeout: Duration) -> Client {
    Client::builder().timeout(timeout).build().unwrap()
}

fn raw_http_status(port: u16, request: &str) -> StatusCode {
    let mut connection = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
    connection
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    connection.write_all(request.as_bytes()).unwrap();
    connection.flush().unwrap();
    let mut status_line = String::new();
    BufReader::new(connection)
        .read_line(&mut status_line)
        .unwrap();
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .and_then(|status| status.parse::<u16>().ok())
        .and_then(|status| StatusCode::from_u16(status).ok());
    status.unwrap_or_else(|| panic!("invalid HTTP status line: {status_line:?}"))
}

fn unused_port() -> u16 {
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let token_file = directory.path().join("http-token");
    fs::write(&token_file, TOKEN).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    (directory, workspace, token_file)
}

#[test]
fn loopback_http_does_not_add_an_undocumented_transaction_size_limit() {
    let (_directory, workspace, token_file) = fixture();
    let server = HttpServer::start(&workspace, &token_file);
    let initialized = server.post(&call(1, "specmesh_init", json!({})));
    assert_eq!(initialized.status(), StatusCode::OK);

    let response = server.post(&call(
        2,
        "specmesh_set",
        json!({
            "transaction":{"operations":[{
                "op":"create",
                "resource":"result",
                "data":{"key":"LARGE_RESULT","content":{"blob":"x".repeat(1_100_000)}}
            }]},
            "control":{"dry_run":true}
        }),
    ));
    assert_eq!(response.status(), StatusCode::OK);
    let response: Value = response.json().unwrap();
    assert_eq!(response["result"]["structuredContent"]["status"], "success");
}

fn assert_tree_omits_secret(path: &Path, secret: &[u8]) {
    for entry in fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let file_type = entry.file_type().unwrap();
        if file_type.is_dir() {
            assert_tree_omits_secret(&entry.path(), secret);
        } else if file_type.is_file() {
            let contents = fs::read(entry.path()).unwrap();
            assert!(
                !contents
                    .windows(secret.len())
                    .any(|window| window == secret),
                "HTTP bearer token leaked into {}",
                entry.path().display()
            );
        }
    }
}

fn initialize(id: u64) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"initialize",
        "params":{
            "protocolVersion":"2025-03-26",
            "capabilities":{},
            "clientInfo":{"name":"integration-test","version":"1"}
        }
    })
}

fn call(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"tools/call",
        "params":{"name":name,"arguments":arguments}
    })
}

fn call_with_progress(id: u64, name: &str, arguments: Value) -> Value {
    json!({
        "jsonrpc":"2.0",
        "id":id,
        "method":"tools/call",
        "params":{
            "name":name,
            "arguments":arguments,
            "_meta":{"progressToken":format!("progress-{id}")}
        }
    })
}

fn specmesh_binary() -> &'static Path {
    static BINARY: OnceLock<PathBuf> = OnceLock::new();
    BINARY
        .get_or_init(|| {
            let server_root = Path::new(env!("CARGO_MANIFEST_DIR"));
            let engine_manifest = server_root.join("../specmesh-engine/Cargo.toml");
            let target_directory = std::env::var_os("SPECMESH_STDIO_ENGINE_TARGET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| server_root.join("target/stdio-engine"));
            let output = Command::new(env!("CARGO"))
                .args([
                    "build",
                    "--locked",
                    "--quiet",
                    "--manifest-path",
                    engine_manifest.to_str().unwrap(),
                    "--bin",
                    "specmesh",
                    "--target-dir",
                    target_directory.to_str().unwrap(),
                ])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "cannot build the real stdio MCP binary: stdout={}; stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            target_directory.join("debug/specmesh")
        })
        .as_path()
}

fn stdio_requests(workspace: &Path, requests: &[Value]) -> Vec<Value> {
    let mut child = Command::new(specmesh_binary())
        .args(["--workspace", workspace.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    for request in requests {
        writeln!(input, "{request}").unwrap();
    }
    drop(input);
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "stdio MCP failed: stdout={}; stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty(), "stdio MCP wrote diagnostics");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn stdio_response(workspace: &Path, request: &Value) -> Value {
    let id = request["id"].clone();
    stdio_requests(workspace, std::slice::from_ref(request))
        .into_iter()
        .find(|message| message.get("id") == Some(&id))
        .expect("stdio MCP did not return the requested response")
}

fn stdio_cancel_after_progress(workspace: &Path, request: &Value) -> Value {
    let request_id = request["id"].clone();
    let mut child = Command::new(specmesh_binary())
        .args(["--workspace", workspace.to_str().unwrap(), "mcp"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{request}").unwrap();
    input.flush().unwrap();

    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut cancelled = false;
    let response = loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "stdio MCP ended before returning the cancelled call"
        );
        let message: Value = serde_json::from_str(line.trim()).unwrap();
        if !cancelled && message["method"] == "notifications/progress" {
            writeln!(
                input,
                "{}",
                json!({
                    "jsonrpc":"2.0",
                    "method":"notifications/cancelled",
                    "params":{"requestId":request_id.clone()}
                })
            )
            .unwrap();
            input.flush().unwrap();
            cancelled = true;
        }
        if message.get("id") == Some(&request_id) {
            break message;
        }
    };
    assert!(cancelled, "stdio MCP call completed before cancellation");
    drop(input);
    drop(output);
    let status = child.wait().unwrap();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    assert!(status.success(), "stdio MCP failed: {stderr}");
    assert!(stderr.is_empty(), "stdio MCP wrote diagnostics: {stderr}");
    response
}

fn structured_action(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<Value>().unwrap()["result"]["structuredContent"].clone()
}

fn read_action(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

fn wait_for_action(path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "ActionResult file was not completed: {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10));
    }
    read_action(path)
}

fn assert_object_fields(value: &Value, expected: &[&str]) {
    let actual: BTreeSet<_> = value
        .as_object()
        .unwrap_or_else(|| panic!("expected an object, got {value}"))
        .keys()
        .map(String::as_str)
        .collect();
    let expected: BTreeSet<_> = expected.iter().copied().collect();
    assert_eq!(actual, expected, "unexpected object fields in {value}");
}

fn property_names(value: &Value, names: &mut BTreeSet<String>) {
    match value {
        Value::Object(object) => {
            if let Some(properties) = object.get("properties").and_then(Value::as_object) {
                names.extend(properties.keys().cloned());
            }
            for child in object.values() {
                property_names(child, names);
            }
        }
        Value::Array(array) => {
            for child in array {
                property_names(child, names);
            }
        }
        _ => {}
    }
}

const FORBIDDEN_PUBLIC_FEATURE_FIELDS: &[&str] = &[
    "sample",
    "sampled",
    "sampling",
    "representative",
    "coverage_strategy",
    "t_way",
    "t-way",
    "tway",
    "pairwise",
    "backend",
    "planner",
    "compose",
    "composition",
];

fn assert_cell_task_input_schema_is_closed(tools: &Value) {
    let mut schema_properties = BTreeSet::new();
    property_names(tools, &mut schema_properties);
    for forbidden in FORBIDDEN_PUBLIC_FEATURE_FIELDS {
        assert!(
            !schema_properties.contains(*forbidden),
            "shared tool schema exposed {forbidden}"
        );
    }
    assert!(schema_properties.contains("priority"));

    let get = tools
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "specmesh_get")
        .unwrap();
    let get_properties: BTreeSet<_> = get["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    for forbidden in [
        "sort",
        "order",
        "focus",
        "weight",
        "weights",
        "risk",
        "cost",
        "business_value",
        "learned",
        "personalized",
        "strategy",
    ] {
        assert!(
            !get_properties.contains(forbidden),
            "specmesh_get schema exposed CellTask control {forbidden}"
        );
    }
}

#[test]
fn shared_engine_tool_schema_omits_cell_task_selection_controls() {
    assert_cell_task_input_schema_is_closed(&specmesh_engine::mcp::tools());
}

#[test]
fn loopback_http_enforces_boundaries_and_serves_shared_mcp_surface() {
    let (_directory, workspace, token_file) = fixture();
    let server = HttpServer::start(&workspace, &token_file);
    let client = client();

    let missing = client
        .post(&server.endpoint)
        .header(CONTENT_TYPE, "application/json")
        .body(initialize(1).to_string())
        .send()
        .unwrap();
    assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(missing.headers()[WWW_AUTHENTICATE], "Bearer");
    let missing_body = missing.text().unwrap();
    assert!(!missing_body.contains(TOKEN));

    let wrong = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, "Bearer definitely-wrong")
        .header(CONTENT_TYPE, "application/json")
        .body(initialize(2).to_string())
        .send()
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.headers()[WWW_AUTHENTICATE], "Bearer");
    let wrong_body = wrong.text().unwrap();
    assert!(!wrong_body.contains(TOKEN));
    assert_eq!(missing_body, wrong_body);

    let missing_host = raw_http_status(
        server.port,
        &format!("GET /mcp HTTP/1.1\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n"),
    );
    assert_eq!(missing_host, StatusCode::FORBIDDEN);

    let invalid_host_precedes_auth_and_method = client
        .get(&server.endpoint)
        .header(HOST, format!("localhost:{}", server.port))
        .send()
        .unwrap();
    assert_eq!(
        invalid_host_precedes_auth_and_method.status(),
        StatusCode::FORBIDDEN
    );

    let invalid_origin_precedes_auth_and_method = client
        .get(&server.endpoint)
        .header(ORIGIN, "null")
        .send()
        .unwrap();
    assert_eq!(
        invalid_origin_precedes_auth_and_method.status(),
        StatusCode::FORBIDDEN
    );

    let missing_auth_precedes_method = client.get(&server.endpoint).send().unwrap();
    assert_eq!(
        missing_auth_precedes_method.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        missing_auth_precedes_method.headers()[WWW_AUTHENTICATE],
        "Bearer"
    );

    let bad_host = client
        .post(&server.endpoint)
        .header(HOST, format!("localhost:{}", server.port))
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .body(initialize(3).to_string())
        .send()
        .unwrap();
    assert_eq!(bad_host.status(), StatusCode::FORBIDDEN);

    for origin in ["null", "https://127.0.0.1", "http://localhost"] {
        let response = client
            .post(&server.endpoint)
            .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header(ORIGIN, origin)
            .json(&initialize(4))
            .send()
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    let valid_origin = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(ORIGIN, format!("http://127.0.0.1:{}", server.port))
        .json(&initialize(5))
        .send()
        .unwrap();
    assert_eq!(valid_origin.status(), StatusCode::OK);
    assert!(valid_origin.headers().get("mcp-session-id").is_none());
    assert_eq!(
        valid_origin.json::<Value>().unwrap()["result"]["protocolVersion"],
        "2025-03-26"
    );

    let get = client
        .get(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("accept", "application/json")
        .send()
        .unwrap();
    assert_eq!(get.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(get.headers()["allow"], "POST");

    let other_path = client
        .post(format!("http://127.0.0.1:{}/other", server.port))
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .json(&initialize(59))
        .send()
        .unwrap();
    assert_eq!(other_path.status(), StatusCode::NOT_FOUND);

    let wrong_type = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(initialize(6).to_string())
        .send()
        .unwrap();
    assert_eq!(wrong_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let encoded = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .header("content-encoding", "gzip")
        .body(initialize(61).to_string())
        .send()
        .unwrap();
    assert_eq!(encoded.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    let wrong_accept = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json")
        .body(initialize(60).to_string())
        .send()
        .unwrap();
    assert_eq!(wrong_accept.status(), StatusCode::NOT_ACCEPTABLE);

    let malformed = client
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .header("accept", "application/json, text/event-stream")
        .body("{")
        .send()
        .unwrap();
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(malformed.json::<Value>().unwrap()["error"]["code"], -32700);

    let short = server.post(&call(7, "specmesh_doctor", json!({})));
    assert_eq!(short.status(), StatusCode::OK);
    assert!(
        short.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert!(short.headers().get("mcp-session-id").is_none());
    assert_eq!(
        short.json::<Value>().unwrap()["result"]["structuredContent"]["status"],
        "success"
    );

    let list = server.post(&json!({
        "jsonrpc":"2.0","id":8,"method":"tools/list","params":{}
    }));
    let list = list.json::<Value>().unwrap();
    let schemas = serde_json::to_string(&list["result"]["tools"]).unwrap();
    assert!(!schemas.contains("\"workspace\""));
    assert!(!schemas.contains("delete_input_after_success"));
    assert!(!schemas.contains("authorize_input_deletion"));
    assert!(
        !workspace.join(".specmesh").exists(),
        "HTTP boundary and protocol-only requests must not initialize or mutate the Workspace"
    );

    let initialized = server.post(&call(9, "specmesh_init", json!({})));
    assert_eq!(
        initialized.json::<Value>().unwrap()["result"]["structuredContent"]["status"],
        "success"
    );
    let mesh_before = fs::read(workspace.join(".specmesh/mesh.json")).unwrap();
    let forbidden_workspace = server.post(&call(
        10,
        "specmesh_get",
        json!({"resource":"mesh","workspace":workspace}),
    ));
    let forbidden_workspace = forbidden_workspace.json::<Value>().unwrap();
    assert_eq!(forbidden_workspace["error"]["code"], -32602);
    assert_eq!(
        fs::read(workspace.join(".specmesh/mesh.json")).unwrap(),
        mesh_before
    );

    let forbidden_output = workspace.parent().unwrap().join("adapter-output.json");
    for (id, name, arguments) in [
        (
            101,
            "specmesh_get",
            json!({"resource":"mesh","control":{"workspace":workspace}}),
        ),
        (
            102,
            "specmesh_get",
            json!({"resource":"mesh","control":{"output":forbidden_output}}),
        ),
        (
            103,
            "specmesh_set",
            json!({"transaction":{"operations":[]},"file":"transaction.json"}),
        ),
        (
            104,
            "specmesh_set",
            json!({
                "transaction":{"operations":[]},
                "control":{"delete_input_after_success":true}
            }),
        ),
        (
            105,
            "specmesh_analyze",
            json!({"control":{"overwrite":true,"timeout_ms":0}}),
        ),
    ] {
        let rejected = server.post(&call(id, name, arguments));
        let rejected = rejected.json::<Value>().unwrap();
        assert_eq!(rejected["error"]["code"], -32602);
        assert!(rejected.get("result").is_none());
    }
    assert!(!forbidden_output.exists());
    assert_eq!(
        fs::read(workspace.join(".specmesh/mesh.json")).unwrap(),
        mesh_before
    );

    let token_before = fs::read(&token_file).unwrap();
    let protected = server.post(&call(
        11,
        "specmesh_analyze",
        json!({"control":{"result_output":token_file}}),
    ));
    let protected = protected.json::<Value>().unwrap();
    assert_eq!(
        protected["result"]["structuredContent"]["errors"][0]["code"],
        "INVALID_INPUT"
    );
    assert_eq!(fs::read(&token_file).unwrap(), token_before);

    let token_alias = token_file.with_extension("hard-link");
    fs::hard_link(&token_file, &token_alias).unwrap();
    let protected_identity = server.post(&call(
        12,
        "specmesh_set",
        json!({
            "transaction":{"operations":[{
                "op":"create",
                "resource":"result",
                "data":{"key":"MUST_NOT_COMMIT","content":{}}
            }]},
            "control":{"mesh_output":token_alias}
        }),
    ));
    let protected_identity = protected_identity.json::<Value>().unwrap();
    assert_eq!(
        protected_identity["result"]["structuredContent"]["errors"][0]["code"],
        "INVALID_INPUT"
    );
    assert_eq!(fs::read(&token_file).unwrap(), token_before);
    assert_eq!(
        fs::read(workspace.join(".specmesh/mesh.json")).unwrap(),
        mesh_before
    );

    let token_symlink = token_file.with_extension("symbolic-link");
    symlink(&token_file, &token_symlink).unwrap();
    let protected_symlink = server.post(&call(
        13,
        "specmesh_analyze",
        json!({
            "control":{"result_output":token_symlink,"overwrite":true}
        }),
    ));
    let protected_symlink = protected_symlink.json::<Value>().unwrap();
    assert_eq!(
        protected_symlink["result"]["structuredContent"]["errors"][0]["code"],
        "INVALID_INPUT"
    );
    assert_eq!(fs::read(&token_file).unwrap(), token_before);
    assert_tree_omits_secret(&workspace, TOKEN.as_bytes());
}

#[test]
fn real_stdio_and_http_share_complete_schema_and_typed_action_results() {
    let (directory, workspace, token_file) = fixture();
    let server = HttpServer::start(&workspace, &token_file);
    let list_request = json!({
        "jsonrpc":"2.0","id":14,"method":"tools/list","params":{}
    });
    let http_list: Value = server.post(&list_request).json().unwrap();
    let stdio_list = stdio_response(&workspace, &list_request);
    assert_eq!(http_list, stdio_list);
    let tool_names: Vec<_> = http_list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        tool_names,
        [
            "specmesh_init",
            "specmesh_get",
            "specmesh_set",
            "specmesh_analyze",
            "specmesh_archive",
            "specmesh_doctor"
        ]
    );
    assert_cell_task_input_schema_is_closed(&http_list["result"]["tools"]);

    let forbidden_requests: Vec<_> = [
        (140, "sort", json!("potential")),
        (141, "order", json!("descending")),
        (142, "focus", json!({"dimension":"D"})),
        (143, "weight", json!(10)),
        (144, "strategy", json!("representative")),
        (145, "risk", json!("high")),
        (146, "cost", json!(10)),
        (147, "business_value", json!(10)),
        (148, "learned", json!(true)),
        (149, "personalized", json!(true)),
    ]
    .into_iter()
    .map(|(id, field, value)| {
        let mut arguments = json!({"resource":"tasks","view":"cells"});
        arguments
            .as_object_mut()
            .unwrap()
            .insert(field.to_owned(), value);
        call(id, "specmesh_get", arguments)
    })
    .collect();
    let mut http_rejections: Vec<Value> = forbidden_requests
        .iter()
        .map(|request| server.post(request).json().unwrap())
        .collect();
    let mut stdio_rejections = stdio_requests(&workspace, &forbidden_requests);
    http_rejections.sort_by_key(|response| response["id"].as_u64().unwrap());
    stdio_rejections.sort_by_key(|response| response["id"].as_u64().unwrap());
    assert_eq!(http_rejections, stdio_rejections);
    for rejected in http_rejections {
        assert_eq!(rejected["error"]["code"], -32602, "{rejected}");
        assert!(rejected.get("result").is_none(), "{rejected}");
    }

    let failed_request = call(15, "specmesh_get", json!({"resource":"workspace"}));
    let http_failed: Value = server.post(&failed_request).json().unwrap();
    let stdio_failed = stdio_response(&workspace, &failed_request);
    assert_eq!(http_failed, stdio_failed);
    assert_eq!(http_failed["result"]["isError"], true);
    let failed = &http_failed["result"]["structuredContent"];
    assert_eq!(failed["status"], "failed");
    assert_eq!(failed["errors"][0]["code"], "WORKSPACE_NOT_FOUND");
    assert!(failed.get("data").is_none());

    write_rule_review_mesh(&workspace);

    let direct_review_request = call(
        16,
        "specmesh_get",
        json!({"resource":"rules","view":"review"}),
    );
    let http_direct_review: Value = server.post(&direct_review_request).json().unwrap();
    let stdio_direct_review = stdio_response(&workspace, &direct_review_request);
    assert_eq!(http_direct_review, stdio_direct_review);
    assert_eq!(http_direct_review["result"]["isError"], false);
    let direct_review = &http_direct_review["result"]["structuredContent"];
    assert_eq!(direct_review["status"], "success");
    assert_eq!(direct_review["data"]["items"].as_array().unwrap().len(), 5);
    let direct_rule = &direct_review["data"]["items"][0];
    assert_object_fields(
        direct_rule,
        &[
            "rule",
            "matched_coordinates",
            "ignored_by_exact",
            "overlaps",
        ],
    );
    assert_eq!(direct_rule["matched_coordinates"]["total"], 36);
    assert_eq!(
        direct_rule["matched_coordinates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    assert_eq!(direct_rule["ignored_by_exact"]["total"], 1);
    assert_eq!(direct_rule["overlaps"]["total"], 10);
    assert_eq!(
        direct_rule["overlaps"]["items"].as_array().unwrap().len(),
        5
    );

    let http_review_output = directory.path().join("http-rule-reviews.json");
    let stdio_review_output = directory.path().join("stdio-rule-reviews.json");
    let http_full_review_request = call(
        17,
        "specmesh_get",
        json!({
            "resource":"rules",
            "view":"review",
            "control":{"result_output":http_review_output}
        }),
    );
    let stdio_full_review_request = call(
        17,
        "specmesh_get",
        json!({
            "resource":"rules",
            "view":"review",
            "control":{"result_output":stdio_review_output}
        }),
    );
    let http_full_review: Value = server.post(&http_full_review_request).json().unwrap();
    let stdio_full_review = stdio_response(&workspace, &stdio_full_review_request);
    assert_eq!(http_full_review, stdio_full_review);
    let http_review_file = read_action(&http_review_output);
    let stdio_review_file = read_action(&stdio_review_output);
    assert_eq!(http_review_file, stdio_review_file);
    assert_eq!(
        http_review_file["data"]["items"].as_array().unwrap().len(),
        6
    );
    let full_rule = &http_review_file["data"]["items"][0];
    assert_eq!(
        full_rule["matched_coordinates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        36
    );
    assert_eq!(full_rule["overlaps"]["items"].as_array().unwrap().len(), 10);

    let transaction = json!({"operations":[
        {"op":"create","resource":"constraint","data":{
            "key":"BLOCK_D5","type":"forbid","where":"D=V5"
        }},
        {"op":"create","resource":"rule","data":{
            "key":"CANDIDATE","when":"*","priority":500,"result":"RESULT"
        }}
    ]});
    let direct_confirmation_request = call(
        18,
        "specmesh_set",
        json!({"transaction":transaction.clone()}),
    );
    let http_direct_confirmation: Value = server.post(&direct_confirmation_request).json().unwrap();
    let stdio_direct_confirmation = stdio_response(&workspace, &direct_confirmation_request);
    assert_eq!(http_direct_confirmation, stdio_direct_confirmation);
    assert_eq!(http_direct_confirmation["result"]["isError"], false);
    let confirmation = &http_direct_confirmation["result"]["structuredContent"];
    assert_eq!(confirmation["status"], "confirmation_required");
    assert_eq!(
        confirmation["confirmation"]["reasons"],
        json!(["CONSTRAINT_EXCLUSION_WILL_CHANGE"])
    );
    let impact = &confirmation["data"]["impact"];
    assert!(impact.get("rule_reviews").is_none());
    assert_eq!(impact["constraint_exclusion_changes"]["added"]["total"], 6);
    assert_eq!(
        impact["constraint_exclusion_changes"]["added"]["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    let candidate_review = &confirmation["data"]["rule_reviews"]["items"][0];
    assert_object_fields(
        candidate_review,
        &[
            "rule",
            "matched_coordinates",
            "ignored_by_exact",
            "overlaps",
        ],
    );
    assert_eq!(candidate_review["matched_coordinates"]["total"], 30);
    assert_eq!(candidate_review["overlaps"]["total"], 12);
    assert_eq!(
        candidate_review["overlaps"]["items"]
            .as_array()
            .unwrap()
            .len(),
        5
    );

    let http_confirmation_output = directory.path().join("http-confirmation.json");
    let stdio_confirmation_output = directory.path().join("stdio-confirmation.json");
    let http_full_confirmation_request = call(
        19,
        "specmesh_set",
        json!({
            "transaction":transaction.clone(),
            "control":{"result_output":http_confirmation_output}
        }),
    );
    let stdio_full_confirmation_request = call(
        19,
        "specmesh_set",
        json!({
            "transaction":transaction,
            "control":{"result_output":stdio_confirmation_output}
        }),
    );
    let http_full_confirmation: Value =
        server.post(&http_full_confirmation_request).json().unwrap();
    let stdio_full_confirmation = stdio_response(&workspace, &stdio_full_confirmation_request);
    assert_eq!(http_full_confirmation, stdio_full_confirmation);
    let http_confirmation_file = read_action(&http_confirmation_output);
    let stdio_confirmation_file = read_action(&stdio_confirmation_output);
    assert_eq!(http_confirmation_file, stdio_confirmation_file);
    assert_eq!(http_confirmation_file["status"], "confirmation_required");
    assert_eq!(
        http_confirmation_file["data"]["impact"]["constraint_exclusion_changes"]["added"]["items"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
    let full_candidate = &http_confirmation_file["data"]["rule_reviews"]["items"][0];
    assert_eq!(
        full_candidate["matched_coordinates"]["items"]
            .as_array()
            .unwrap()
            .len(),
        30
    );
    assert_eq!(
        full_candidate["overlaps"]["items"]
            .as_array()
            .unwrap()
            .len(),
        12
    );
}

#[test]
fn real_stdio_and_http_share_rejected_action_result_direct_and_complete_output() {
    let (directory, workspace, token_file) = fixture();
    write_results_mesh(&workspace, 1);
    let server = HttpServer::start(&workspace, &token_file);
    let transaction = json!({"operations":[{
        "op":"delete",
        "resource":"result",
        "target":{}
    }]});

    let direct_request = call(
        27,
        "specmesh_set",
        json!({"transaction":transaction.clone()}),
    );
    let http_direct: Value = server.post(&direct_request).json().unwrap();
    let stdio_direct = stdio_response(&workspace, &direct_request);
    assert_eq!(http_direct, stdio_direct);
    assert!(http_direct.get("error").is_none());
    assert_eq!(http_direct["result"]["isError"], false);
    let direct_action = http_direct["result"]["structuredContent"].clone();
    assert_object_fields(&direct_action, &["status", "errors"]);
    assert_eq!(direct_action["status"], "rejected");
    assert_eq!(direct_action["errors"].as_array().unwrap().len(), 1);
    assert_object_fields(&direct_action["errors"][0], &["code", "message", "path"]);
    assert_eq!(direct_action["errors"][0]["code"], "INVALID_INPUT");
    assert_eq!(direct_action["errors"][0]["path"], "/operations/0/target");
    assert!(direct_action.get("confirmation").is_none());
    assert!(direct_action.get("data").is_none());
    assert!(direct_action.get("impact").is_none());

    let http_output = directory.path().join("http-rejected.json");
    let stdio_output = directory.path().join("stdio-rejected.json");
    let http_full_request = call(
        28,
        "specmesh_set",
        json!({
            "transaction":transaction.clone(),
            "control":{"result_output":http_output}
        }),
    );
    let stdio_full_request = call(
        28,
        "specmesh_set",
        json!({
            "transaction":transaction,
            "control":{"result_output":stdio_output}
        }),
    );
    let http_full: Value = server.post(&http_full_request).json().unwrap();
    let stdio_full = stdio_response(&workspace, &stdio_full_request);
    assert_eq!(http_full, stdio_full);
    assert_eq!(http_full["result"]["structuredContent"], direct_action);
    assert_eq!(read_action(&http_output), direct_action);
    assert_eq!(read_action(&stdio_output), direct_action);
}

#[test]
fn real_stdio_cancellation_and_http_disconnect_share_the_cancelled_action_result() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 15, 60);
    let server = HttpServer::start(&workspace, &token_file);

    let http_output = directory.path().join("http-cancelled.json");
    let http_request = call_with_progress(
        25,
        "specmesh_analyze",
        json!({"control":{"result_output":http_output}}),
    );
    let connection = open_until_progress(server.port, &http_request);
    connection.shutdown(Shutdown::Both).unwrap();
    let http_action = wait_for_action(&http_output);

    let stdio_output = directory.path().join("stdio-cancelled.json");
    let stdio_request = call_with_progress(
        26,
        "specmesh_analyze",
        json!({"control":{"result_output":stdio_output}}),
    );
    let stdio_response = stdio_cancel_after_progress(&workspace, &stdio_request);
    let stdio_action = stdio_response["result"]["structuredContent"].clone();
    assert_eq!(stdio_response["result"]["isError"], true);
    assert_eq!(http_action, stdio_action);
    assert_eq!(read_action(&stdio_output), stdio_action);
    assert_eq!(http_action["status"], "failed");
    assert_eq!(http_action["errors"][0]["code"], "ACTION_CANCELLED");
    assert!(http_action.get("data").is_none());
}

#[test]
fn result_outputs_share_complete_application_projection_across_http_and_stdio() {
    let (directory, workspace, token_file) = fixture();
    write_results_mesh(&workspace, 8);
    let server = HttpServer::start(&workspace, &token_file);

    let http_output = directory.path().join("http-results.json");
    let arguments = json!({
        "resource":"results",
        "control":{"result_output":http_output}
    });
    let direct = structured_action(server.post(&call(20, "specmesh_get", arguments)));
    let complete = read_action(&http_output);
    assert_eq!(direct["status"], "success");
    assert_eq!(direct["data"]["items"].as_array().unwrap().len(), 5);
    assert_eq!(complete["data"]["items"].as_array().unwrap().len(), 8);
    assert_eq!(direct["data"]["mesh_hash"], complete["data"]["mesh_hash"]);
    assert_eq!(direct["data"]["page"], complete["data"]["page"]);
    assert_eq!(complete["data"]["page"]["total"]["value"], 8);
    assert_eq!(complete["data"]["page"]["total"]["count_is_exact"], true);
    assert!(direct["data"].get("result_output").is_none());

    let stdio_output = directory.path().join("stdio-results.json");
    let stdio = stdio_response(
        &workspace,
        &call(
            21,
            "specmesh_get",
            json!({
                "resource":"results",
                "control":{"result_output":stdio_output}
            }),
        ),
    );
    assert_eq!(stdio["result"]["structuredContent"], direct);
    assert_eq!(read_action(&stdio_output), complete);

    let original = fs::read(&http_output).unwrap();
    let exists = structured_action(server.post(&call(
        22,
        "specmesh_get",
        json!({
            "resource":"results",
            "control":{"result_output":http_output}
        }),
    )));
    assert_eq!(exists["status"], "rejected");
    assert_eq!(exists["errors"][0]["code"], "OUTPUT_ALREADY_EXISTS");
    assert_eq!(fs::read(&http_output).unwrap(), original);

    let overwritten = structured_action(server.post(&call(
        23,
        "specmesh_get",
        json!({
            "resource":"results",
            "control":{"result_output":http_output,"overwrite":true}
        }),
    )));
    assert_eq!(overwritten, direct);
    assert_eq!(read_action(&http_output), complete);

    let mixed_output = directory.path().join("mixed-results.json");
    let mixed = structured_action(server.post(&call(
        24,
        "specmesh_get",
        json!({
            "resource":"results",
            "page":1,
            "control":{"result_output":mixed_output}
        }),
    )));
    assert_eq!(mixed["status"], "rejected");
    assert_eq!(mixed["errors"][0]["code"], "INVALID_INPUT");
    assert_eq!(read_action(&mixed_output), mixed);
}

#[test]
fn analyze_and_archive_result_outputs_use_shared_terminal_semantics() {
    let (directory, workspace, token_file) = fixture();
    write_results_mesh(&workspace, 2);
    let server = HttpServer::start(&workspace, &token_file);

    let analyze_output = directory.path().join("analyze-result.json");
    let analyze = structured_action(server.post(&call(
        30,
        "specmesh_analyze",
        json!({"control":{"result_output":analyze_output}}),
    )));
    assert_eq!(read_action(&analyze_output), analyze);

    let mesh_before = fs::read(workspace.join(".specmesh/mesh.json")).unwrap();
    let set_output = directory.path().join("set-result.json");
    let set = structured_action(server.post(&call(
        33,
        "specmesh_set",
        json!({
            "transaction":{"operations":[{
                "op":"create",
                "resource":"result",
                "data":{"key":"DRY_RUN_RESULT","content":{"ok":true}}
            }]},
            "control":{"dry_run":true,"result_output":set_output}
        }),
    )));
    assert_eq!(set["status"], "success");
    assert_eq!(read_action(&set_output), set);
    assert_eq!(
        fs::read(workspace.join(".specmesh/mesh.json")).unwrap(),
        mesh_before
    );

    let archive_output = directory.path().join("archive-result.json");
    let archived = structured_action(server.post(&call(
        31,
        "specmesh_archive",
        json!({
            "requirements_version":"result-output-success",
            "control":{"result_output":archive_output}
        }),
    )));
    assert_eq!(archived["status"], "success");
    assert_object_fields(
        &archived["data"],
        &["requirements_version", "archived_at", "mesh_hash"],
    );
    assert_eq!(
        archived["data"]["requirements_version"],
        "result-output-success"
    );
    assert_eq!(read_action(&archive_output), archived);
    let archive_path = workspace.join(".specmesh/archives/result-output-success.json");
    assert!(archive_path.is_file());
    let archive_document: Value =
        serde_json::from_slice(&fs::read(&archive_path).unwrap()).unwrap();
    assert_object_fields(
        &archive_document,
        &[
            "requirements_version",
            "archived_at",
            "mesh_hash",
            "mesh",
            "analysis",
        ],
    );
    assert_object_fields(
        &archive_document["analysis"],
        &["counts", "maintenance_tasks", "analysis_diagnostics"],
    );
    assert_eq!(
        archive_document["requirements_version"],
        archived["data"]["requirements_version"]
    );
    assert_eq!(
        archive_document["archived_at"],
        archived["data"]["archived_at"]
    );
    assert_eq!(archive_document["mesh_hash"], archived["data"]["mesh_hash"]);
    assert!(archive_document["analysis"].get("mesh_hash").is_none());
    assert!(archive_document["analysis"].get("conflicts").is_none());
    assert!(archive_document["analysis"].get("rule_overlaps").is_none());

    let archives =
        structured_action(server.post(&call(34, "specmesh_get", json!({"resource":"archives"}))));
    assert_eq!(archives["status"], "success");
    assert_object_fields(&archives["data"], &["items", "page"]);
    assert_object_fields(
        &archives["data"]["items"][0],
        &["requirements_version", "archived_at", "mesh_hash"],
    );
    assert_eq!(archives["data"]["items"][0], archived["data"]);
    assert!(archives["data"].get("mesh_hash").is_none());

    let read_only_parent = directory.path().join("read-only-result-parent");
    fs::create_dir(&read_only_parent).unwrap();
    fs::set_permissions(&read_only_parent, fs::Permissions::from_mode(0o500)).unwrap();
    let candidate = read_only_parent.join("archive-warning.json");
    let result_output = if fs::write(&candidate, b"permission probe").is_err() {
        candidate
    } else {
        fs::remove_file(&candidate).unwrap();
        ["/proc", "/sys", "/System"]
            .into_iter()
            .map(|parent| {
                Path::new(parent).join(format!(
                    "specmesh-http-result-{}-{}.json",
                    std::process::id(),
                    server.port
                ))
            })
            .find(|path| {
                if !path.parent().is_some_and(Path::is_dir) {
                    return false;
                }
                match fs::write(path, b"probe") {
                    Err(_) => true,
                    Ok(()) => {
                        fs::remove_file(path).unwrap();
                        false
                    }
                }
            })
            .expect("test needs a directory that permits metadata but rejects file creation")
    };
    let warning = structured_action(server.post(&call(
        32,
        "specmesh_archive",
        json!({
            "requirements_version":"post-commit-warning",
            "control":{"result_output":result_output}
        }),
    )));
    fs::set_permissions(&read_only_parent, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(warning["status"], "success");
    assert_eq!(warning["warnings"].as_array().unwrap().len(), 1);
    assert_eq!(warning["warnings"][0]["code"], "RESULT_OUTPUT_WRITE_FAILED");
    assert_eq!(
        warning["warnings"][0]["details"]["committed_output"]["kind"],
        "archive"
    );
    assert_eq!(
        warning["warnings"][0]["details"]["committed_output"]["path"],
        workspace
            .join(".specmesh/archives/post-commit-warning.json")
            .to_string_lossy()
            .as_ref()
    );
    assert_eq!(
        warning["warnings"][0]["details"]["result_output_path"],
        result_output.to_string_lossy().as_ref()
    );
    assert!(!result_output.exists());
    assert!(
        workspace
            .join(".specmesh/archives/post-commit-warning.json")
            .is_file()
    );
}

#[test]
fn loopback_http_calls_are_concurrent_and_shutdown_is_graceful() {
    let (_directory, workspace, token_file) = fixture();
    let mut server = HttpServer::start(&workspace, &token_file);
    let endpoint = server.endpoint.clone();
    let barrier = Arc::new(Barrier::new(12));
    let threads: Vec<_> = (0..12)
        .map(|_| {
            let endpoint = endpoint.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait();
                let response = client()
                    .post(endpoint)
                    .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
                    .header("accept", "application/json, text/event-stream")
                    .json(&call(200, "specmesh_doctor", json!({})))
                    .send()
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK);
                let response = response.json::<Value>().unwrap();
                assert_eq!(response["id"], 200);
                assert_eq!(response["result"]["structuredContent"]["status"], "success");
            })
        })
        .collect();
    for thread in threads {
        thread.join().unwrap();
    }
    server.interrupt_and_wait();
}

#[test]
fn loopback_http_admits_exactly_32_active_calls_and_rejects_33_without_queueing() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 15, 60);
    let server = HttpServer::start(&workspace, &token_file);
    let barrier = Arc::new(Barrier::new(33));
    let result_outputs: Vec<_> = (0..32)
        .map(|index| directory.path().join(format!("admitted-{index:02}.json")))
        .collect();
    let workers: Vec<_> = result_outputs
        .iter()
        .enumerate()
        .map(|(index, result_output)| {
            let barrier = barrier.clone();
            let request = call_with_progress(
                300 + index as u64,
                "specmesh_analyze",
                json!({"control":{"result_output":result_output}}),
            );
            let port = server.port;
            thread::spawn(move || {
                barrier.wait();
                open_until_progress(port, &request)
            })
        })
        .collect();
    barrier.wait();
    let connections: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(connections.len(), 32);
    assert!(result_outputs.iter().all(|path| !path.exists()));

    let overload_output = directory.path().join("overload-must-not-run.json");
    let overload_id = json!("overload/第33");
    let overload_request = json!({
        "jsonrpc":"2.0",
        "id":overload_id,
        "method":"tools/call",
        "params":{
            "name":"specmesh_analyze",
            "arguments":{"control":{"result_output":overload_output}}
        }
    });
    let overload = client_with_timeout(Duration::from_secs(5))
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("accept", "application/json, text/event-stream")
        .json(&overload_request)
        .send()
        .expect("the 33rd call was queued instead of being rejected immediately");
    assert_eq!(overload.status(), StatusCode::OK);
    assert_eq!(
        overload.headers()[CONTENT_TYPE].to_str().unwrap(),
        "application/json"
    );
    assert_eq!(
        overload.json::<Value>().unwrap(),
        json!({
            "jsonrpc":"2.0",
            "id":overload_id,
            "error":{
                "code":-32000,
                "message":"too many MCP tool calls are active"
            }
        })
    );
    assert!(!overload_output.exists());
    assert!(result_outputs.iter().all(|path| !path.exists()));

    for connection in connections {
        connection.shutdown(Shutdown::Both).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while result_outputs.iter().any(|path| !path.exists()) {
        assert!(
            Instant::now() < deadline,
            "an admitted call did not terminate after its connection was released"
        );
        thread::sleep(Duration::from_millis(10));
    }
    for path in &result_outputs {
        let result = read_action(path);
        assert_eq!(result["status"], "failed", "{}", path.display());
        assert_eq!(
            result["errors"][0]["code"],
            "ACTION_CANCELLED",
            "{}",
            path.display()
        );
        assert!(result.get("data").is_none(), "{}", path.display());
    }

    let recovered = structured_action(server.post(&call(399, "specmesh_doctor", json!({}))));
    assert_eq!(recovered["status"], "success");
}

#[test]
fn loopback_http_reads_the_caller_token_exactly_once() {
    let (_directory, workspace, token_file) = fixture();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o400)).unwrap();
    let server = HttpServer::start(&workspace, &token_file);

    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&token_file, ROTATED_TOKEN).unwrap();
    let original = server.post(&initialize(70));
    assert_eq!(original.status(), StatusCode::OK);

    let rotated = client()
        .post(&server.endpoint)
        .header(AUTHORIZATION, format!("Bearer {ROTATED_TOKEN}"))
        .header(CONTENT_TYPE, "application/json")
        .body(initialize(71).to_string())
        .send()
        .unwrap();
    assert_eq!(rotated.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(rotated.headers()[WWW_AUTHENTICATE], "Bearer");
}

#[test]
fn loopback_http_binds_one_canonical_workspace_without_parent_search_or_rebind() {
    let directory = tempfile::tempdir().unwrap();
    write_results_mesh(directory.path(), 1);
    let parent_mesh_before = fs::read(directory.path().join(".specmesh/mesh.json")).unwrap();
    let workspace = directory.path().join("bound-workspace");
    let nested = workspace.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let alias = nested.join("..");
    let canonical = fs::canonicalize(&workspace).unwrap();
    let token_file = directory.path().join("bound-workspace-token");
    fs::write(&token_file, TOKEN).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let server = HttpServer::start(&alias, &token_file);

    let not_found =
        structured_action(server.post(&call(80, "specmesh_get", json!({"resource":"workspace"}))));
    assert_eq!(not_found["status"], "failed");
    assert_eq!(not_found["errors"][0]["code"], "WORKSPACE_NOT_FOUND");
    assert_eq!(
        not_found["errors"][0]["details"]["workspace_path"],
        canonical.to_string_lossy().as_ref()
    );

    let initialized = structured_action(server.post(&call(81, "specmesh_init", json!({}))));
    assert_eq!(initialized["status"], "success");
    let workspace_result =
        structured_action(server.post(&call(82, "specmesh_get", json!({"resource":"workspace"}))));
    assert_object_fields(
        &workspace_result["data"],
        &["workspace_path", "mesh_path", "mesh_hash", "schema_version"],
    );
    assert_eq!(
        workspace_result["data"]["workspace_path"],
        canonical.to_string_lossy().as_ref()
    );

    let rebind = server.post(&call(
        83,
        "specmesh_get",
        json!({"resource":"workspace","workspace":directory.path()}),
    ));
    assert_eq!(rebind.json::<Value>().unwrap()["error"]["code"], -32602);
    assert_eq!(
        fs::read(directory.path().join(".specmesh/mesh.json")).unwrap(),
        parent_mesh_before
    );
}

#[test]
fn omitted_workspace_binds_the_process_start_directory() {
    let directory = tempfile::tempdir().unwrap();
    write_results_mesh(directory.path(), 1);
    let parent_mesh_before = fs::read(directory.path().join(".specmesh/mesh.json")).unwrap();
    let workspace = directory.path().join("process-current-directory");
    fs::create_dir(&workspace).unwrap();
    let canonical = fs::canonicalize(&workspace).unwrap();
    let token_file = directory.path().join("cwd-token");
    fs::write(&token_file, TOKEN).unwrap();
    fs::set_permissions(&token_file, fs::Permissions::from_mode(0o600)).unwrap();
    let server = HttpServer::start_in_current_directory(&workspace, &token_file);

    let not_found =
        structured_action(server.post(&call(84, "specmesh_get", json!({"resource":"workspace"}))));
    assert_eq!(not_found["status"], "failed");
    assert_eq!(not_found["errors"][0]["code"], "WORKSPACE_NOT_FOUND");
    assert_eq!(
        not_found["errors"][0]["details"]["workspace_path"],
        canonical.to_string_lossy().as_ref()
    );

    let initialized = structured_action(server.post(&call(85, "specmesh_init", json!({}))));
    assert_eq!(initialized["status"], "success");
    let bound =
        structured_action(server.post(&call(86, "specmesh_get", json!({"resource":"workspace"}))));
    assert_eq!(
        bound["data"]["workspace_path"],
        canonical.to_string_lossy().as_ref()
    );
    assert_eq!(
        fs::read(directory.path().join(".specmesh/mesh.json")).unwrap(),
        parent_mesh_before
    );
}

#[test]
fn completed_long_post_streams_progress_then_one_final_result() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (_directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 15, 25);
    let server = HttpServer::start(&workspace, &token_file);

    let timed_out = structured_action(server.post(&call(
        89,
        "specmesh_analyze",
        json!({"control":{"timeout_ms":1}}),
    )));
    assert_eq!(timed_out["status"], "failed");
    assert_eq!(timed_out["errors"][0]["code"], "ACTION_TIMEOUT");
    assert_eq!(timed_out["errors"][0]["details"]["timeout_ms"], 1);

    let json_started = Instant::now();
    let no_progress = server.post(&call(88, "specmesh_analyze", json!({})));
    assert!(json_started.elapsed() >= Duration::from_secs(2));
    assert!(
        no_progress.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    assert_eq!(
        no_progress.json::<Value>().unwrap()["result"]["structuredContent"]["status"],
        "success"
    );

    let started = Instant::now();
    let response = server.post(&call_with_progress(90, "specmesh_analyze", json!({})));
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("mcp-session-id").is_none());
    assert!(
        response.headers()[CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let body = response.text().unwrap();
    assert!(started.elapsed() >= Duration::from_secs(2));
    assert!(!body.lines().any(|line| line.starts_with("id:")));
    let events: Vec<Value> = body
        .lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect();
    let (final_event, progress_events) = events
        .split_last()
        .expect("SSE response must contain a final JSON-RPC response");
    assert!(!progress_events.is_empty());
    let mut previous_current = None;
    for event in progress_events {
        assert_eq!(event["method"], "notifications/progress");
        let payload = &event["params"]["value"];
        assert_eq!(payload["type"], "progress");
        assert_eq!(payload["phase"], "count_valid_cells");
        let current = payload["current"]
            .as_u64()
            .expect("progress current must be a JSON uint");
        if let Some(previous) = previous_current {
            assert!(
                current > previous,
                "progress current must strictly increase"
            );
        }
        previous_current = Some(current);
        assert!(payload.get("sequence").is_none());
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| event.get("result").is_some() || event.get("error").is_some())
            .count(),
        1,
        "SSE response must contain exactly one final JSON-RPC response"
    );
    assert_eq!(final_event["id"], 90);
    assert_eq!(
        final_event["result"]["structuredContent"]["status"],
        "success"
    );
}

#[test]
fn long_post_uses_sse_disconnect_cancels_only_that_call_and_lock_is_shared() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (_directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 15, 60);
    let server = HttpServer::start(&workspace, &token_file);

    let long_archive = call_with_progress(
        100,
        "specmesh_archive",
        json!({"requirements_version":"disconnect-test"}),
    );
    let connection = open_until_progress(server.port, &long_archive);

    let competing = server.post(&call(
        101,
        "specmesh_archive",
        json!({"requirements_version":"competing"}),
    ));
    assert_eq!(competing.status(), StatusCode::OK);
    let competing = competing.json::<Value>().unwrap();
    assert_eq!(
        competing["result"]["structuredContent"]["errors"][0]["code"],
        "LOCK_BUSY"
    );
    assert_eq!(
        competing["result"]["structuredContent"]["errors"][0]["retryable"],
        true
    );

    connection.shutdown(Shutdown::Both).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(workspace.join(".specmesh/workspace.lock"))
        .unwrap();
    loop {
        match FileExt::try_lock_exclusive(&lock_file) {
            Ok(()) => {
                FileExt::unlock(&lock_file).unwrap();
                break;
            }
            Err(error) => {
                assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
            }
        }
        assert!(
            Instant::now() < deadline,
            "disconnected HTTP call did not release the Workspace lock"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !workspace
            .join(".specmesh/archives/disconnect-test.json")
            .exists(),
        "the disconnected archive call crossed its commit point"
    );

    let independent = server.post(&call(103, "specmesh_doctor", json!({})));
    assert_eq!(
        independent.json::<Value>().unwrap()["result"]["structuredContent"]["status"],
        "success"
    );
}

#[test]
fn shutdown_cancels_an_in_flight_post_and_waits_for_its_commit_boundary() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (_directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 15, 60);
    let mut server = HttpServer::start(&workspace, &token_file);
    let archive = call_with_progress(
        110,
        "specmesh_archive",
        json!({"requirements_version":"shutdown-test"}),
    );
    let connection = open_until_progress(server.port, &archive);

    server.interrupt_and_wait();
    drop(connection);
    assert!(
        !workspace
            .join(".specmesh/archives/shutdown-test.json")
            .exists(),
        "shutdown cancellation crossed the archive commit point"
    );
}

#[test]
fn shutdown_after_primary_commit_preserves_success_and_finishes_result_output() {
    let _long_test = LONG_TEST_LOCK
        .lock()
        .unwrap_or_else(|lock| lock.into_inner());
    let (directory, workspace, token_file) = fixture();
    write_large_mesh(&workspace, 12, 0);
    let mut server = HttpServer::start(&workspace, &token_file);
    let result_output = directory.path().join("post-commit-result.json");
    let request = call_with_progress(
        111,
        "specmesh_set",
        json!({
            "transaction":{"operations":[{
                "op":"create",
                "resource":"constraint",
                "data":{"key":"POST_COMMIT","type":"forbid","where":"*"}
            }]},
            "control":{
                "skip_confirmation":true,
                "result_output":result_output
            }
        }),
    );
    let connection = open_post(server.port, &request);
    let mesh_path = workspace.join(".specmesh/mesh.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mesh = fs::read(&mesh_path).unwrap();
        if mesh
            .windows(b"POST_COMMIT".len())
            .any(|window| window == b"POST_COMMIT")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the set call did not cross its primary Mesh commit point"
        );
        thread::yield_now();
    }
    assert!(
        !result_output.exists(),
        "the call had already crossed its result completion point"
    );

    connection.shutdown(Shutdown::Both).unwrap();
    server.interrupt_and_wait();

    let mesh = fs::read_to_string(&mesh_path).unwrap();
    assert!(mesh.contains("POST_COMMIT"));
    let result = read_action(&result_output);
    assert_eq!(result["status"], "success");
    assert!(result.get("errors").is_none());
    let added = &result["data"]["impact"]["constraint_exclusion_changes"]["added"];
    assert_eq!(added["total"], 4096);
    assert_eq!(added["items"].as_array().unwrap().len(), 4096);
    let warning_codes: Vec<_> = result
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|warning| warning["code"].as_str().unwrap())
        .collect();
    assert!(
        warning_codes.is_empty() || warning_codes == ["CANCELLATION_NOT_APPLIED"],
        "a post-commit stop produced an unexpected terminal warning: {result}"
    );
}

#[test]
fn shutdown_is_not_held_open_by_an_incomplete_authenticated_post_body() {
    let (_directory, workspace, token_file) = fixture();
    let mut server = HttpServer::start(&workspace, &token_file);
    let mut connection = TcpStream::connect((Ipv4Addr::LOCALHOST, server.port)).unwrap();
    write!(
        connection,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: 1024\r\n\r\n{{",
        server.port
    )
    .unwrap();
    connection.flush().unwrap();
    thread::sleep(Duration::from_millis(100));

    server.interrupt_and_wait();
    drop(connection);
}

#[test]
fn invalid_token_files_and_unavailable_ports_fail_without_secret_disclosure() {
    let (directory, workspace, token_file) = fixture();

    let group_readable = directory.path().join("group-readable");
    fs::write(&group_readable, TOKEN).unwrap();
    fs::set_permissions(&group_readable, fs::Permissions::from_mode(0o640)).unwrap();
    assert_startup_fails(&workspace, &group_readable, unused_port());

    let unreadable = directory.path().join("owner-unreadable");
    fs::write(&unreadable, TOKEN).unwrap();
    fs::set_permissions(&unreadable, fs::Permissions::from_mode(0o000)).unwrap();
    assert_startup_fails(&workspace, &unreadable, unused_port());

    let missing = directory.path().join("missing-token");
    assert_startup_fails(&workspace, &missing, unused_port());
    assert!(!missing.exists());

    let token_directory = directory.path().join("token-directory");
    fs::create_dir(&token_directory).unwrap();
    assert_startup_fails(&workspace, &token_directory, unused_port());

    let malformed = directory.path().join("malformed");
    fs::write(&malformed, format!("{TOKEN}\n")).unwrap();
    fs::set_permissions(&malformed, fs::Permissions::from_mode(0o600)).unwrap();
    assert_startup_fails(&workspace, &malformed, unused_port());

    let invalid_base64 = directory.path().join("invalid-base64");
    fs::write(&invalid_base64, format!("!{}", "A".repeat(42))).unwrap();
    fs::set_permissions(&invalid_base64, fs::Permissions::from_mode(0o400)).unwrap();
    assert_startup_fails(&workspace, &invalid_base64, unused_port());

    let inside = workspace.join("token");
    fs::write(&inside, TOKEN).unwrap();
    fs::set_permissions(&inside, fs::Permissions::from_mode(0o600)).unwrap();
    assert_startup_fails(&workspace, &inside, unused_port());

    let link = directory.path().join("token-link");
    symlink(&token_file, &link).unwrap();
    assert_startup_fails(&workspace, &link, unused_port());

    let non_normalized = workspace.join("..").join("http-token");
    assert_startup_fails(&workspace, &non_normalized, unused_port());

    let relative = Path::new("relative-http-token");
    assert_startup_fails(&workspace, relative, unused_port());

    let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = occupied.local_addr().unwrap().port();
    assert_startup_fails(&workspace, &token_file, port);

    let zero = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "--port",
            "0",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!zero.status.success());
    assert!(!String::from_utf8_lossy(&zero.stderr).contains(TOKEN));

    let host_override = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "--port",
            &unused_port().to_string(),
            "--token-file",
            token_file.to_str().unwrap(),
            "--host",
            "0.0.0.0",
        ])
        .output()
        .unwrap();
    assert!(!host_override.status.success());
    assert!(!String::from_utf8_lossy(&host_override.stderr).contains(TOKEN));

    let help = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(help.status.success());
    let help = String::from_utf8(help.stdout).unwrap();
    for option in ["--workspace", "--port", "--token-file"] {
        assert!(
            help.contains(option),
            "server help omitted {option}: {help}"
        );
    }
    assert!(!help.contains("--host"));
    assert!(!help.contains("--timeout"));
    assert!(!help.split_ascii_whitespace().any(|word| word == "--token"));

    for arguments in [
        vec!["--port", "12345"],
        vec!["--token-file", token_file.to_str().unwrap()],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
    }

    let oversized_port = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .args([
            "--port",
            "65536",
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(oversized_port.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&oversized_port.stderr).contains(TOKEN));

    let startup_timeout_port = unused_port();
    let startup_timeout = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "--timeout",
            "1ms",
            "--port",
            &startup_timeout_port.to_string(),
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert_eq!(startup_timeout.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&startup_timeout.stderr).contains(TOKEN));
    let rebound = TcpListener::bind((Ipv4Addr::LOCALHOST, startup_timeout_port)).unwrap();
    drop(rebound);
}

fn assert_startup_fails(workspace: &Path, token_file: &Path, port: u16) {
    let output = Command::new(env!("CARGO_BIN_EXE_specmesh-server"))
        .args([
            "--workspace",
            workspace.to_str().unwrap(),
            "--port",
            &port.to_string(),
            "--token-file",
            token_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stdout).contains(TOKEN));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(TOKEN));
}

fn open_post(endpoint_port: u16, message: &Value) -> TcpStream {
    let mut socket = TcpStream::connect((Ipv4Addr::LOCALHOST, endpoint_port)).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let body = message.to_string();
    write!(
        socket,
        "POST /mcp HTTP/1.1\r\nHost: 127.0.0.1:{endpoint_port}\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nAccept: application/json, text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    socket.flush().unwrap();
    socket
}

fn open_until_progress(endpoint_port: u16, message: &Value) -> TcpStream {
    let socket = open_post(endpoint_port, message);
    let mut response = BufReader::new(socket);
    let mut saw_sse = false;
    loop {
        let mut line = String::new();
        let read = response.read_line(&mut line).unwrap();
        assert_ne!(read, 0, "long MCP call ended before emitting progress");
        if line
            .to_ascii_lowercase()
            .starts_with("content-type: text/event-stream")
        {
            saw_sse = true;
        }
        if line.contains("notifications/progress") {
            let value = line
                .split_once("data: ")
                .map(|(_, data)| data.trim())
                .and_then(|data| serde_json::from_str::<Value>(data).ok())
                .expect("progress SSE data is JSON");
            let payload = &value["params"]["value"];
            assert_eq!(payload["type"], "progress");
            assert!(payload["phase"].is_string());
            assert!(payload["current"].is_u64());
            assert!(payload.get("sequence").is_none());
            break;
        }
    }
    assert!(saw_sse);
    response.into_inner()
}

fn write_results_mesh(workspace: &Path, result_count: usize) {
    let specmesh = workspace.join(".specmesh");
    fs::create_dir(&specmesh).unwrap();
    let results: Vec<_> = (0..result_count)
        .map(|index| {
            json!({
                "id":index + 1,
                "key":format!("RESULT_{index:02}"),
                "content":{"index":index},
                "sources":[]
            })
        })
        .collect();
    fs::write(
        specmesh.join("mesh.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version":2,
            "next_id":result_count + 1,
            "sources":[],
            "dimensions":[],
            "constraints":[],
            "results":results,
            "rules":[],
            "exact_cells":[]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_rule_review_mesh(workspace: &Path) {
    let d_values: Vec<_> = (0..6)
        .map(|index| {
            json!({
                "id":index + 2,
                "key":format!("V{index}"),
                "attributes":{},
                "sources":[]
            })
        })
        .collect();
    let e_values: Vec<_> = (0..6)
        .map(|index| {
            json!({
                "id":index + 9,
                "key":format!("W{index}"),
                "attributes":{},
                "sources":[]
            })
        })
        .collect();
    let rules: Vec<_> = (0..6)
        .map(|index| {
            json!({
                "id":index + 16,
                "key":format!("RULE_{index}"),
                "when":"*",
                "priority":500,
                "result":"RESULT",
                "attributes":{},
                "sources":[]
            })
        })
        .collect();
    let specmesh = workspace.join(".specmesh");
    fs::create_dir(&specmesh).unwrap();
    fs::write(
        specmesh.join("mesh.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version":2,
            "next_id":23,
            "sources":[],
            "dimensions":[
                {
                    "id":1,
                    "key":"D",
                    "attributes":{},
                    "sources":[],
                    "values":d_values
                },
                {
                    "id":8,
                    "key":"E",
                    "attributes":{},
                    "sources":[],
                    "values":e_values
                }
            ],
            "constraints":[],
            "results":[{
                "id":15,
                "key":"RESULT",
                "content":{},
                "sources":[]
            }],
            "rules":rules,
            "exact_cells":[{
                "id":22,
                "key":"EXACT",
                "coordinate":{"D":"V0","E":"W0"},
                "result":"RESULT",
                "attributes":{},
                "sources":[]
            }]
        }))
        .unwrap(),
    )
    .unwrap();
}

fn write_large_mesh(workspace: &Path, dimension_count: usize, rule_count: usize) {
    let mut next_id = 1_u64;
    let mut dimensions = Vec::new();
    for dimension_index in 0..dimension_count {
        let id = next_id;
        next_id += 1;
        let mut values = Vec::new();
        for suffix in ['A', 'B'] {
            values.push(json!({
                "id":next_id,
                "key":format!("VALUE_{dimension_index}_{suffix}"),
                "attributes":{},
                "sources":[]
            }));
            next_id += 1;
        }
        dimensions.push(json!({
            "id":id,
            "key":format!("DIM_{dimension_index}"),
            "attributes":{},
            "sources":[],
            "values":values
        }));
    }
    let mut rules = Vec::new();
    for rule_index in 0..rule_count {
        let predicates: Vec<_> = (0..dimension_count)
            .map(|dimension_index| {
                let suffix = if rule_index & (1 << dimension_index) == 0 {
                    'A'
                } else {
                    'B'
                };
                format!("DIM_{dimension_index}=VALUE_{dimension_index}_{suffix}")
            })
            .collect();
        rules.push(json!({
            "id":next_id,
            "key":format!("RULE_{rule_index}"),
            "when":predicates.join(" & "),
            "priority":500,
            "result":"RESULT_MISSING",
            "attributes":{},
            "sources":[]
        }));
        next_id += 1;
    }
    let specmesh = workspace.join(".specmesh");
    fs::create_dir(&specmesh).unwrap();
    fs::write(
        specmesh.join("mesh.json"),
        serde_json::to_vec_pretty(&json!({
            "schema_version":2,
            "next_id":next_id,
            "sources":[],
            "dimensions":dimensions,
            "constraints":[],
            "results":[],
            "rules":rules,
            "exact_cells":[]
        }))
        .unwrap(),
    )
    .unwrap();
}
