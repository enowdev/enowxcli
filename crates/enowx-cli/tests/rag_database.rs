use serde_json::{json, Value};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

static NEXT_HOME: AtomicU64 = AtomicU64::new(0);

struct TestHome {
    root: PathBuf,
    home: PathBuf,
    workspace: PathBuf,
    cleanup: bool,
}

impl TestHome {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "enowx-rag-database-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_HOME.fetch_add(1, Ordering::Relaxed)
        ));
        let home = root.join("home");
        let workspace = root.join("workspace");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&workspace).unwrap();
        Self {
            root,
            home,
            workspace,
            cleanup: true,
        }
    }

    fn write_config(&self, value: &Value) {
        fs::write(
            self.home.join("builtin-mcp.json"),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn write_secret(&self, value: &Value) {
        fs::write(
            self.home.join("auth.json"),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }

    fn run(&self, args: &[&str], input: &[u8]) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_enowx"));
        command
            .args(args)
            .env("ENX_HOME", &self.home)
            .env("ENX_WORKSPACE", &self.workspace)
            .current_dir(&self.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        child.wait_with_output().unwrap()
    }

    fn rag_setup(&self, url: &str) -> std::process::Output {
        let args = [
            "mcp",
            "set",
            "rag",
            "--provider",
            "custom",
            "--url",
            url,
            "--model",
            "test-embedding-3",
            "--dimension",
            "3",
            "--auto-index",
            "off",
        ];
        let supported = embedded_supported_here();
        self.run(
            &args,
            if supported {
                b""
            } else {
                b"postgres://default.invalid/db\n"
            },
        )
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        if self.cleanup {
            let metadata = self.home.join("rag-db/endpoint.json");
            if metadata.exists() {
                wait_for_metadata_removed(&self.home, Duration::from_secs(100));
            }
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

fn embedded_supported_here() -> bool {
    cfg!(any(
        all(
            target_arch = "x86_64",
            target_os = "linux",
            target_env = "gnu"
        ),
        all(
            target_arch = "aarch64",
            target_os = "linux",
            target_env = "gnu"
        ),
        all(
            target_arch = "aarch64",
            target_vendor = "apple",
            target_os = "macos"
        ),
        all(
            target_arch = "x86_64",
            target_os = "windows",
            target_env = "msvc"
        )
    ))
}

/// A subprocess harness with bounded protocol reads. A hung request panics,
/// unwinds through Drop, and always kills and waits for its child process.
struct McpProcess {
    child: Child,
    input: ChildStdin,
    replies: mpsc::Receiver<std::io::Result<String>>,
    next_id: u64,
}

impl McpProcess {
    fn start(test: &TestHome) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_enowx"))
            .args(["mcp", "serve", "rag"])
            .env("ENX_HOME", &test.home)
            .env("ENX_WORKSPACE", &test.workspace)
            .current_dir(&test.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (sender, replies) = mpsc::channel();
        thread::spawn(move || read_replies(output, sender));
        Self {
            child,
            input,
            replies,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0", "id":id, "method":method, "params":params})
        )
        .unwrap();
        self.input.flush().unwrap();
        let line = self
            .replies
            .recv_timeout(Duration::from_secs(180))
            .unwrap_or_else(|error| {
                panic!("MCP request {method} exceeded 180-second deadline: {error}")
            })
            .unwrap_or_else(|error| panic!("reading MCP reply for {method}: {error}"));
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], id, "unexpected MCP reply: {reply}");
        reply
    }

    fn call(&mut self, tool: &str, arguments: Value) -> String {
        let reply = self.request("tools/call", json!({"name":tool, "arguments":arguments}));
        assert_eq!(
            reply["result"]["isError"], false,
            "MCP tool failed: {reply}"
        );
        reply["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    fn tools(&mut self) -> Value {
        self.request("tools/list", json!({}))
    }
}

fn read_replies(output: ChildStdout, sender: mpsc::Sender<std::io::Result<String>>) {
    for line in BufReader::new(output).lines() {
        if sender.send(line).is_err() {
            return;
        }
    }
}

impl Drop for McpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Minimal local OpenAI-compatible endpoint returning deterministic 3D vectors.
struct FakeEmbeddings {
    url: String,
    address: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FakeEmbeddings {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let url = format!("http://{address}/v1");
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = thread::spawn(move || {
            while !stopping.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => handle_embedding(&mut stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url,
            address,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for FakeEmbeddings {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(&self.address);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_embedding(stream: &mut TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                request.extend_from_slice(&buffer[..n]);
                if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n")
                {
                    break position + 4;
                }
            }
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    while request.len() < header_end + content_length {
        match stream.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(n) => request.extend_from_slice(&buffer[..n]),
        }
    }
    let body: Value =
        serde_json::from_slice(&request[header_end..header_end + content_length]).unwrap();
    let count = body["input"].as_array().map_or(1, Vec::len);
    let data: Vec<Value> = (0..count)
        .map(|index| json!({"index":index,"embedding":[1.0,0.0,0.0]}))
        .collect();
    let response = serde_json::to_vec(&json!({"data":data})).unwrap();
    let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
    let _ = stream.write_all(&response);
    let _ = stream.flush();
}

fn output_text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        output_text(output)
    );
}

fn wait_for_metadata_removed(home: &Path, timeout: Duration) {
    let metadata = home.join("rag-db/endpoint.json");
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !metadata.exists() {
            return;
        }
        thread::sleep(Duration::from_secs(2));
    }
    panic!("embedded database owner did not remove endpoint metadata after idle timeout");
}

/// Native-runner CI selects this exact test for real embedded pgvector smoke.
#[test]
fn embedded_vector_smoke() {
    let fake = FakeEmbeddings::start();
    let home = TestHome::new("e2e");
    fs::write(
        home.workspace.join("src.rs"),
        "fn deterministic_rag_fixture() { let vector = 3; }\n",
    )
    .unwrap();
    fs::write(
        home.workspace.join("other.rs"),
        "fn second_deterministic_fixture() { let index = 2; }\n",
    )
    .unwrap();

    let setup = home.rag_setup(&fake.url);
    if !embedded_supported_here() {
        assert_success(&setup);
        let config: Value =
            serde_json::from_slice(&fs::read(home.home.join("builtin-mcp.json")).unwrap()).unwrap();
        assert_eq!(config["rag"]["database"], "postgres");
        assert!(output_text(&setup).contains("PostgreSQL"));
        let before = fs::read(home.home.join("auth.json")).unwrap();
        let rejected = home.run(
            &[
                "mcp",
                "set",
                "rag",
                "--database",
                "embedded",
                "--provider",
                "custom",
                "--url",
                &fake.url,
                "--model",
                "test-embedding-3",
                "--dimension",
                "3",
            ],
            b"",
        );
        assert!(!rejected.status.success());
        assert!(output_text(&rejected).contains("not supported"));
        assert_eq!(before, fs::read(home.home.join("auth.json")).unwrap());
        return;
    }

    assert_success(&setup);
    let config: Value =
        serde_json::from_slice(&fs::read(home.home.join("builtin-mcp.json")).unwrap()).unwrap();
    assert_eq!(config["rag"]["database"], "embedded");
    assert!(!fs::read_to_string(home.home.join("auth.json"))
        .unwrap_or_default()
        .contains("mcp-rag-dsn"));
    let listing = home.run(&["mcp", "list"], b"");
    assert_success(&listing);
    assert!(output_text(&listing).contains("embedded PostgreSQL"));
    assert!(!output_text(&listing).contains("postgres://"));

    let dsn = "postgres://dsn-selection-secret.invalid/db";
    let dsn_setup = home.run(
        &[
            "mcp",
            "set",
            "rag",
            "--dsn",
            dsn,
            "--provider",
            "custom",
            "--url",
            &fake.url,
            "--model",
            "test-embedding-3",
            "--dimension",
            "3",
        ],
        b"",
    );
    assert_success(&dsn_setup);
    let config: Value =
        serde_json::from_slice(&fs::read(home.home.join("builtin-mcp.json")).unwrap()).unwrap();
    assert_eq!(config["rag"]["database"], "postgres");
    assert!(!output_text(&dsn_setup).contains(dsn));
    let listing = home.run(&["mcp", "list"], b"");
    assert_success(&listing);
    assert!(output_text(&listing).contains("PostgreSQL"));
    assert!(!output_text(&listing).contains(dsn));

    // Embedded with a DSN fails before mutating credentials.
    home.write_secret(&json!({"mcp-rag-dsn":{"type":"api","key":"postgres://secret.invalid/db"}}));
    let before = fs::read(home.home.join("auth.json")).unwrap();
    let conflict = home.run(
        &[
            "mcp",
            "set",
            "rag",
            "--database",
            "embedded",
            "--dsn",
            "postgres://replacement.invalid/db",
            "--provider",
            "custom",
            "--url",
            &fake.url,
            "--model",
            "test-embedding-3",
            "--dimension",
            "3",
        ],
        b"",
    );
    assert!(!conflict.status.success());
    assert_eq!(before, fs::read(home.home.join("auth.json")).unwrap());
    assert!(!output_text(&conflict).contains("postgres://secret.invalid/db"));

    // Legacy setup omits database, but its stored DSN selects PostgreSQL.
    home.write_config(&json!({"rag":{"provider":"custom","base_url":fake.url,"model":"test-embedding-3","dimension":3,"rerank":"off"}}));
    let legacy = home.run(&["mcp", "list"], b"");
    assert_success(&legacy);
    assert!(output_text(&legacy).contains("PostgreSQL"));
    assert!(!output_text(&legacy).contains("secret.invalid"));

    // Explicit PostgreSQL with no secret prompts for a DSN.
    home.write_secret(&json!({}));
    let prompted_dsn = "postgres://prompted.invalid/db";
    let prompt = home.run(
        &[
            "mcp",
            "set",
            "rag",
            "--database",
            "postgres",
            "--provider",
            "custom",
            "--url",
            &fake.url,
            "--model",
            "test-embedding-3",
            "--dimension",
            "3",
        ],
        format!("{prompted_dsn}\n").as_bytes(),
    );
    assert_success(&prompt);
    let auth: Value =
        serde_json::from_slice(&fs::read(home.home.join("auth.json")).unwrap()).unwrap();
    assert_eq!(auth["mcp-rag-dsn"]["key"], prompted_dsn);
    assert!(!output_text(&prompt).contains(prompted_dsn));

    // Restore a fresh, legacy setup so absence of backend and DSN selects Embedded.
    home.write_config(&json!({"rag":{"provider":"custom","base_url":fake.url,"model":"test-embedding-3","dimension":3,"rerank":"off","auto_index":false}}));
    home.write_secret(&json!({}));
    assert_success(&home.rag_setup(&fake.url));
    let config: Value =
        serde_json::from_slice(&fs::read(home.home.join("builtin-mcp.json")).unwrap()).unwrap();
    assert_eq!(config["rag"]["database"], "embedded");

    let mut first = McpProcess::start(&home);
    let tools = first.tools();
    let names: Vec<&str> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    assert!(names.contains(&"index") && names.contains(&"search") && names.contains(&"forget"));
    assert!(!names.contains(&"rag-db"));
    let indexed = first.call("index", json!({"path":home.workspace}));
    assert!(indexed.contains("2 files, 2 chunks"), "{indexed}");
    let search = first.call(
        "search",
        json!({"path":home.workspace,"query":"deterministic rag fixture vector"}),
    );
    assert!(search.contains("src.rs:1-1"), "{search}");
    assert!(search.contains("deterministic_rag_fixture"), "{search}");
    drop(first);

    // A second process sees both persisted rows after the first process exits.
    let mut second = McpProcess::start(&home);
    let status: Value =
        serde_json::from_str(&second.call("status", json!({"path":home.workspace}))).unwrap();
    assert_eq!(status["chunks"], 2);
    drop(second);

    // Concurrent independent index and forget calls serialize at project scope.
    let a_home = home.home.clone();
    let a_workspace = home.workspace.clone();
    let b_home = home.home.clone();
    let b_workspace = home.workspace.clone();
    let index_thread = thread::spawn(move || {
        let test = TestHome {
            root: PathBuf::new(),
            home: a_home,
            workspace: a_workspace,
            cleanup: false,
        };
        McpProcess::start(&test).call("index", json!({"path":test.workspace}))
    });
    let forget_thread = thread::spawn(move || {
        let test = TestHome {
            root: PathBuf::new(),
            home: b_home,
            workspace: b_workspace,
            cleanup: false,
        };
        McpProcess::start(&test).call("forget", json!({"path":test.workspace}))
    });
    assert!(index_thread.join().unwrap().contains("2 files, 2 chunks"));
    assert!(forget_thread.join().unwrap().contains("Removed"));
    let mut check = McpProcess::start(&home);
    let status_text = check.call("status", json!({"path":home.workspace}));
    let status: Value = serde_json::from_str(&status_text).unwrap();
    assert!(
        status["chunks"] == 0 || status["chunks"] == 2,
        "unexpected final state: {status}"
    );
    if status["chunks"] == 0 {
        check.call("index", json!({"path":home.workspace}));
    }
    drop(check);

    // Owner stops after the idle period; a fresh client starts it over the same cluster.
    wait_for_metadata_removed(&home.home, Duration::from_secs(100));
    let mut reopened = McpProcess::start(&home);
    let status_text = reopened.call("status", json!({"path":home.workspace}));
    let status: Value = serde_json::from_str(&status_text).unwrap();
    assert_eq!(status["chunks"], 2);
    drop(reopened);
    wait_for_metadata_removed(&home.home, Duration::from_secs(100));
}
