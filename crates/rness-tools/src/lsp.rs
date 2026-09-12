//! Optional read-only stdio LSP navigation, with one connection per server/workspace.
use async_trait::async_trait;
use reqwest::Url;
use rness_engine::tools::{Tool, ToolRegistry};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub servers: BTreeMap<String, Server>,
    #[serde(default = "timeout")]
    pub timeout_ms: u64,
    #[serde(default = "locations")]
    pub max_locations: usize,
    #[serde(default = "chars")]
    pub max_result_chars: usize,
}
fn timeout() -> u64 {
    60_000
}
fn locations() -> usize {
    100
}
fn chars() -> usize {
    16_000
}
#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Server {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    pub extension_to_language: BTreeMap<String, String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub initialization_options: Value,
    #[serde(default)]
    pub configuration: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    operation: Operation,
    file_path: String,
    line: u32,
    character: u32,
}
#[derive(Clone, Copy, Deserialize)]
enum Operation {
    #[serde(rename = "goToDefinition")]
    Definition,
    #[serde(rename = "findReferences")]
    References,
    #[serde(rename = "goToImplementation")]
    Implementation,
    #[serde(rename = "hover")]
    Hover,
}
impl Operation {
    fn method(self) -> &'static str {
        match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Implementation => "textDocument/implementation",
            Self::Hover => "textDocument/hover",
        }
    }
    fn capability(self) -> &'static str {
        match self {
            Self::Definition => "definitionProvider",
            Self::References => "referencesProvider",
            Self::Implementation => "implementationProvider",
            Self::Hover => "hoverProvider",
        }
    }
}
type Slot = Arc<tokio::sync::Mutex<Option<ManagedConnection>>>;
struct ManagedConnection(Option<Connection>);
impl std::ops::Deref for ManagedConnection {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        self.0.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for ManagedConnection {
    fn deref_mut(&mut self) -> &mut Connection {
        self.0.as_mut().unwrap()
    }
}
impl Drop for ManagedConnection {
    fn drop(&mut self) {
        let Some(mut connection) = self.0.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                connection.close().await;
            });
        }
        // Without a runtime, Child::kill_on_drop remains the last-resort cleanup.
    }
}
struct Runtime {
    config: Config,
    connections: Mutex<BTreeMap<(String, PathBuf), Slot>>,
}
struct LspTool {
    runtime: Arc<Runtime>,
    workspace: Option<PathBuf>,
}
pub fn register(registry: &ToolRegistry, config: Config) -> Result<(), String> {
    if config.servers.is_empty()
        || config.timeout_ms == 0
        || config.max_locations == 0
        || config.max_result_chars < 128
    {
        return Err("LSP needs servers and positive limits (max_result_chars >=128)".into());
    }
    let mut extensions = std::collections::HashSet::new();
    for (name, server) in &config.servers {
        if name.is_empty() || server.command.is_empty() || server.extension_to_language.is_empty() {
            return Err("LSP server needs name, command and extension mapping".into());
        }
        for (extension, language) in &server.extension_to_language {
            if !extension.starts_with('.')
                || extension.len() < 2
                || extension.to_lowercase() != *extension
                || extension.contains(['/', '\\'])
                || language.is_empty()
                || !extensions.insert(extension)
            {
                return Err(format!("invalid or conflicting LSP extension: {extension}"));
            }
        }
    }
    registry.try_register(Arc::new(LspTool {
        runtime: Arc::new(Runtime {
            config,
            connections: Mutex::new(BTreeMap::new()),
        }),
        workspace: None,
    }))
}
#[async_trait]
impl Tool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }
    fn description(&self) -> &str {
        "Precise read-only language-server navigation: goToDefinition, findReferences, goToImplementation, hover. line and character are one-based UTF-16 cursor coordinates. References include declarations. Use search/read for ordinary navigation. Requires an explicitly configured server and session workspace."
    }
    fn input_schema(&self) -> Value {
        json!({"type":"object","properties":{"operation":{"type":"string","enum":["goToDefinition","findReferences","goToImplementation","hover"]},"file_path":{"type":"string"},"line":{"type":"integer","minimum":1},"character":{"type":"integer","minimum":1}},"required":["operation","file_path","line","character"],"additionalProperties":false})
    }
    fn for_workspace(&self, _: &String, workspace: &Path) -> Option<Arc<dyn Tool>> {
        Some(Arc::new(Self {
            runtime: self.runtime.clone(),
            workspace: Some(workspace.into()),
        }))
    }
    async fn execute(&self, args: Value) -> Result<String, String> {
        self.run(args, &CancellationToken::new()).await
    }
    async fn execute_call(
        &self,
        _: &String,
        _: &str,
        args: Value,
        cancel: &CancellationToken,
    ) -> Result<String, String> {
        self.run(args, cancel).await
    }
}
impl LspTool {
    async fn run(&self, args: Value, cancel: &CancellationToken) -> Result<String, String> {
        tokio::select! {
            biased;
            _ = cancel.cancelled()=>Err("LSP cancelled".into()),
            result = tokio::time::timeout(Duration::from_millis(self.runtime.config.timeout_ms),self.query(args))=> result.map_err(|_|"LSP timed out".to_string())?,
        }
    }
    async fn query(&self, args: Value) -> Result<String, String> {
        let args: Args = serde_json::from_value(args).map_err(|e| e.to_string())?;
        if args.line == 0 || args.character == 0 {
            return Err("LSP coordinates must be one-based UTF-16".into());
        }
        let root = tokio::fs::canonicalize(
            self.workspace
                .as_ref()
                .ok_or("LSP requires session workspace")?,
        )
        .await
        .map_err(|e| e.to_string())?;
        let path = tokio::fs::canonicalize(root.join(&args.file_path))
            .await
            .map_err(|e| e.to_string())?;
        let extension = format!(
            ".{}",
            path.extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_lowercase()
        );
        let (name, server) = self
            .runtime
            .config
            .servers
            .iter()
            .find(|(_, s)| s.extension_to_language.contains_key(&extension))
            .ok_or_else(|| format!("LSP unavailable for {extension}"))?;
        let mut source = Vec::new();
        tokio::fs::File::open(&path)
            .await
            .map_err(|e| e.to_string())?
            .take(4_000_001)
            .read_to_end(&mut source)
            .await
            .map_err(|e| e.to_string())?;
        if source.len() > 4_000_000 {
            return Err("LSP document exceeds 4 MB".into());
        }
        let source = String::from_utf8(source).map_err(|_| "LSP source must be UTF-8")?;
        let line = source
            .split('\n')
            .nth((args.line - 1) as usize)
            .ok_or("LSP line outside document")?
            .trim_end_matches('\r');
        let column = (args.character - 1) as usize;
        let mut offset = 0;
        let mut boundary = column == 0;
        for c in line.chars() {
            offset += c.len_utf16();
            boundary |= offset == column;
        }
        if !boundary {
            return Err("LSP character outside line or inside surrogate pair".into());
        }
        let uri = Url::from_file_path(&path)
            .map_err(|_| "invalid document path")?
            .to_string();
        let root_uri = Url::from_directory_path(&root)
            .map_err(|_| "invalid workspace path")?
            .to_string();
        let slot = self
            .runtime
            .connections
            .lock()
            .unwrap()
            .entry((name.clone(), root.clone()))
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(None)))
            .clone();
        let mut slot = slot.lock().await;
        // Keep the active connection outside the slot: cancellation drops/kills it rather than reusing a partial exchange.
        let mut connection = match slot.take() {
            Some(c) => c,
            None => Connection::start(server, &root, &root_uri).await?,
        };
        if connection
            .child
            .try_wait()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            connection = Connection::start(server, &root, &root_uri).await?;
        }
        let cap = &connection.capabilities[args.operation.capability()];
        if !cap.is_object() && cap != true {
            *slot = Some(connection);
            return Err("LSP server does not support this operation".into());
        }
        connection.send(json!({"jsonrpc":"2.0","method":"textDocument/didOpen","params":{"textDocument":{"uri":uri,"languageId":server.extension_to_language[&extension],"version":1,"text":source}}})).await?;
        let mut params = json!({"textDocument":{"uri":uri},"position":{"line":args.line-1,"character":args.character-1}});
        if matches!(args.operation, Operation::References) {
            params["context"] = json!({"includeDeclaration":true});
        }
        let response = connection.request(args.operation.method(), params).await;
        let closed=connection.send(json!({"jsonrpc":"2.0","method":"textDocument/didClose","params":{"textDocument":{"uri":uri}}})).await;
        let response = response?;
        closed?;
        *slot = Some(connection);
        render(
            args.operation,
            response,
            &root_uri,
            self.runtime.config.max_locations,
            self.runtime.config.max_result_chars,
        )
    }
}
struct Connection {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: u64,
    capabilities: Value,
    configuration: Value,
    root_uri: String,
    pending: Option<u64>,
    writable: bool,
    synchronized: bool,
    initialized: bool,
}
impl Connection {
    async fn close(&mut self) {
        // A cancelled write cannot safely be followed by another JSON-RPC frame.
        let graceful = async {
            if !self.writable {
                return;
            }
            if let Some(id) = self.pending {
                let _ = self
                    .send(json!({"jsonrpc":"2.0","method":"$/cancelRequest","params":{"id":id}}))
                    .await;
                let _ = tokio::time::timeout(Duration::from_millis(100), self.child.wait()).await;
            }
            // A cancelled read may have consumed a partial frame. Never resume that parser.
            if self.initialized && self.synchronized && self.pending.is_none() {
                if self.request("shutdown", Value::Null).await.is_ok() {
                    let _ = self.send(json!({"jsonrpc":"2.0","method":"exit"})).await;
                    let _ = self.child.wait().await;
                }
            }
        };
        let _ = tokio::time::timeout(Duration::from_secs(1), graceful).await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
    async fn start(
        server: &Server,
        root: &Path,
        root_uri: &str,
    ) -> Result<ManagedConnection, String> {
        let mut command = tokio::process::Command::new(&server.command);
        command.args(&server.args).current_dir(root).env_clear();
        for key in [
            "PATH",
            "HOME",
            "USERPROFILE",
            "SYSTEMROOT",
            "TMPDIR",
            "TEMP",
            "LANG",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| format!("LSP launch {}: {e}", server.command))?;
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut this = ManagedConnection(Some(Self {
            pending: None,
            writable: true,
            synchronized: true,
            initialized: false,
            child,
            input,
            output,
            next: 0,
            capabilities: Value::Null,
            configuration: server.configuration.clone(),
            root_uri: root_uri.into(),
        }));
        let initialized=this.request("initialize",json!({"processId":std::process::id(),"rootUri":root_uri,"capabilities":{"general":{"positionEncodings":["utf-16"]},"workspace":{"configuration":true,"workspaceFolders":true}},"workspaceFolders":[{"uri":root_uri,"name":root.file_name().and_then(|s|s.to_str()).unwrap_or("workspace")}],"initializationOptions":server.initialization_options})).await?;
        this.capabilities = initialized["capabilities"].clone();
        if this
            .capabilities
            .get("positionEncoding")
            .is_some_and(|v| v != "utf-16")
        {
            return Err("LSP server must negotiate UTF-16".into());
        }
        let sync = &this.capabilities["textDocumentSync"];
        if !(sync.as_u64().is_some_and(|n| n == 1 || n == 2) || sync["openClose"] == true) {
            return Err("LSP server must support transient open/close".into());
        }
        this.send(json!({"jsonrpc":"2.0","method":"initialized","params":{}}))
            .await?;
        this.initialized = true;
        Ok(this)
    }
    async fn send(&mut self, value: Value) -> Result<(), String> {
        let bytes = serde_json::to_vec(&value).map_err(|e| e.to_string())?;
        self.writable = false;
        self.input
            .write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        self.input
            .write_all(&bytes)
            .await
            .map_err(|e| e.to_string())?;
        self.input.flush().await.map_err(|e| e.to_string())?;
        self.writable = true;
        Ok(())
    }
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        self.pending = Some(id);
        loop {
            self.synchronized = false;
            let message = read_message(&mut self.output).await?;
            self.synchronized = true;
            if let Some(method) = message["method"].as_str() {
                if let Some(id) = message.get("id") {
                    let result = match method {
                        "workspace/configuration" => Some(Value::Array(
                            message
                                .pointer("/params/items")
                                .and_then(Value::as_array)
                                .ok_or("invalid LSP configuration request")?
                                .iter()
                                .map(|_| self.configuration.clone())
                                .collect(),
                        )),
                        "workspace/workspaceFolders" => {
                            Some(json!([{"uri":self.root_uri,"name":"workspace"}]))
                        }
                        "window/workDoneProgress/create" => Some(Value::Null),
                        "workspace/applyEdit" => {
                            Some(json!({"applied":false,"failureReason":"read-only LSP client"}))
                        }
                        _ => None,
                    };
                    self.send(match result {Some(result)=>json!({"jsonrpc":"2.0","id":id,"result":result}),None=>json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Unsupported client method"}})}).await?;
                }
                continue;
            }
            if message["id"] == id {
                self.pending = None;
                if let Some(error) = message.get("error") {
                    return Err(format!("LSP request error: {error}"));
                }
                return message
                    .get("result")
                    .cloned()
                    .ok_or("invalid LSP response".into());
            }
        }
    }
}
async fn read_message<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Result<Value, String> {
    let mut header = Vec::new();
    loop {
        let byte = reader
            .read_u8()
            .await
            .map_err(|e| format!("LSP transport: {e}"))?;
        header.push(byte);
        if header.len() > 8192 {
            return Err("LSP headers exceed limit".into());
        }
        if header.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let text = std::str::from_utf8(&header).map_err(|_| "invalid LSP headers")?;
    let mut length = None;
    for line in text.split("\r\n").filter(|line| !line.is_empty()) {
        let (key, value) = line.split_once(':').ok_or("invalid LSP header")?;
        if key.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err("duplicate Content-Length".into());
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid Content-Length")?,
            );
        }
    }
    let length = length.ok_or("missing Content-Length")?;
    if length > 16_000_000 {
        return Err("LSP message exceeds 16 MB".into());
    }
    let mut bytes = vec![0; length];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(|e| e.to_string())?;
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn framing_bounds_and_utf16_results() {
        let value = json!({"id":1,"result":"ok"});
        let bytes = serde_json::to_vec(&value).unwrap();
        let frame = format!(
            "Content-Length: {}\r\n\r\n{}",
            bytes.len(),
            String::from_utf8(bytes).unwrap()
        );
        assert_eq!(
            read_message(&mut BufReader::new(frame.as_bytes()))
                .await
                .unwrap(),
            value
        );
        for malformed in [
            "Content-Length: 17000000\r\n\r\n",
            "Content-Length: 1\r\nContent-Length: 1\r\n\r\n0",
        ] {
            assert!(read_message(&mut BufReader::new(malformed.as_bytes()))
                .await
                .is_err());
        }
        let r = json!({"start":{"line":0,"character":2},"end":{"line":0,"character":5}});
        let text = render(
            Operation::Definition,
            json!([{"targetUri":"file:///tmp/a","targetSelectionRange":r}]),
            "file:///tmp/",
            100,
            16000,
        )
        .unwrap();
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["locations"][0]["range"]["start"]["character"], 3);
        assert!(
            render(
                Operation::Hover,
                json!({"contents":"é".repeat(1000)}),
                "file:///tmp/",
                100,
                128
            )
            .unwrap()
            .chars()
            .count()
                <= 128
        );
    }
    #[tokio::test]
    async fn real_stdio_queries_reuse_process_and_refresh_document() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("server.py");
        std::fs::write(&script,r#"
import sys,json
opened=None
while True:
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line==b'\r\n': break
        k,v=line.decode().split(':',1); headers[k.lower()]=v.strip()
    m=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method=m.get('method')
    if method=='textDocument/didOpen': opened=m['params']['textDocument']
    if method=='textDocument/didClose': opened=None
    if 'id' not in m: continue
    if method=='initialize': result={'capabilities':{'textDocumentSync':1,'definitionProvider':True,'referencesProvider':True,'implementationProvider':True,'hoverProvider':True}}
    elif method=='textDocument/hover': result={'contents':opened['text']}
    else:
        assert opened is not None
        assert m['params']['position']=={'line':0,'character':2}
        if method=='textDocument/references': assert m['params']['context']['includeDeclaration']
        result=[{'uri':opened['uri'],'range':{'start':{'line':0,'character':2},'end':{'line':0,'character':3}}}]
    body=json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(body)).encode()+body);sys.stdout.buffer.flush()
"#).unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "😀x").unwrap();
        let config:Config=serde_json::from_value(json!({"servers":{"fixture":{"command":"python3","args":[script],"extension_to_language":{".rs":"rust"}}}})).unwrap();
        let registry = ToolRegistry::default();
        register(&registry, config).unwrap();
        let tool = registry
            .get("lsp")
            .unwrap()
            .for_workspace(&"s".into(), dir.path())
            .unwrap();
        for operation in [
            "goToDefinition",
            "findReferences",
            "goToImplementation",
            "hover",
        ] {
            let output = tool
                .execute(json!({"operation":operation,"file_path":"a.rs","line":1,"character":3}))
                .await
                .unwrap();
            assert!(output.contains(if operation == "hover" {
                "😀x"
            } else {
                "locations"
            }));
        }
        std::fs::write(&file, "new").unwrap();
        assert!(tool
            .execute(json!({"operation":"hover","file_path":"a.rs","line":1,"character":1}))
            .await
            .unwrap()
            .contains("new"));
        assert!(tool
            .execute(json!({"operation":"hover","file_path":"a.rs","line":0,"character":1}))
            .await
            .is_err());
    }
    #[tokio::test]
    async fn lifecycle_shutdown_cancel_timeout_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("lifecycle.py");
        let log = dir.path().join("events");
        std::fs::write(&script, r#"
import sys,json,os
log=sys.argv[1]
def record(text):
    with open(log,'a') as f: f.write(str(os.getpid())+' '+text+'\n')
record('start')
while True:
    headers={}
    while True:
        line=sys.stdin.buffer.readline()
        if not line: sys.exit(0)
        if line==b'\r\n': break
        k,v=line.decode().split(':',1); headers[k.lower()]=v.strip()
    m=json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method=m.get('method');record(method)
    if method=='exit': sys.exit(0)
    if method=='$/cancelRequest':
        record('cancel-id-'+str(m['params']['id']));continue
    if 'id' not in m: continue
    if method=='initialize': result={'capabilities':{'textDocumentSync':1,'hoverProvider':True}}
    elif method=='shutdown': result=None
    elif method=='hang': continue
    elif method=='partial':
        sys.stdout.buffer.write(b'Content-Length: 100\r\n\r\n{"id":');sys.stdout.buffer.flush();continue
    elif method=='crash': sys.exit(1)
    else: result={'contents':'ok'}
    body=json.dumps({'jsonrpc':'2.0','id':m['id'],'result':result}).encode()
    sys.stdout.buffer.write(('Content-Length: %d\r\n\r\n'%len(body)).encode()+body);sys.stdout.buffer.flush()
"#).unwrap();
        let server: Server = serde_json::from_value(
            json!({"command":"python3","args":[script,log],"extension_to_language":{".rs":"rust"}}),
        )
        .unwrap();
        let mut connection = Connection::start(&server, dir.path(), "file:///workspace/")
            .await
            .unwrap();
        connection.close().await;
        let events = std::fs::read_to_string(&log).unwrap();
        assert!(events.contains(" shutdown\n"));
        assert!(events.contains(" exit\n"));
        for method in ["hang", "partial"] {
            let mut connection = Connection::start(&server, dir.path(), "file:///workspace/")
                .await
                .unwrap();
            assert!(tokio::time::timeout(
                Duration::from_millis(100),
                connection.request(method, json!({}))
            )
            .await
            .is_err());
            assert_eq!(connection.pending, Some(2));
            connection.close().await;
            assert!(connection.child.try_wait().unwrap().is_some());
        }
        assert_eq!(std::fs::read_to_string(&log).unwrap().matches("cancel-id-2").count(), 2);
        let mut crashed = Connection::start(&server, dir.path(), "file:///workspace/")
            .await
            .unwrap();
        assert!(crashed.request("crash", json!({})).await.is_err());
        crashed.close().await;
        let mut restarted = Connection::start(&server, dir.path(), "file:///workspace/")
            .await
            .unwrap();
        assert_eq!(
            restarted
                .request("textDocument/hover", json!({}))
                .await
                .unwrap()["contents"],
            "ok"
        );
        restarted.close().await;
    }
    #[test]
    fn duplicate_extensions_rejected_atomically() {
        let config:Config=serde_json::from_value(json!({"servers":{"a":{"command":"x","extension_to_language":{".rs":"rust"}},"b":{"command":"y","extension_to_language":{".rs":"rust"}}}})).unwrap();
        let registry = ToolRegistry::default();
        assert!(register(&registry, config).is_err());
        assert!(registry.get("lsp").is_none());
    }
}

fn range(value: &Value) -> Result<Value, String> {
    let mut result = value.clone();
    for side in ["start", "end"] {
        for key in ["line", "character"] {
            result[side][key] = json!(value[side][key]
                .as_u64()
                .and_then(|n| n.checked_add(1))
                .ok_or("invalid LSP range")?);
        }
    }
    Ok(result)
}
fn hover_text(value: &Value) -> Result<String, String> {
    if let Some(text) = value.as_str() {
        return Ok(text.into());
    }
    if let Some(items) = value.as_array() {
        return items
            .iter()
            .map(hover_text)
            .collect::<Result<Vec<_>, _>>()
            .map(|v| v.join("\n\n"));
    }
    value["value"]
        .as_str()
        .map(str::to_owned)
        .ok_or("invalid hover contents".into())
}
fn render(
    operation: Operation,
    value: Value,
    root: &str,
    max: usize,
    chars: usize,
) -> Result<String, String> {
    let mut result = if matches!(operation, Operation::Hover) {
        if value.is_null() {
            json!({"kind":"hover","hover":null})
        } else {
            let mut hover = json!({"contents":hover_text(&value["contents"])?});
            if let Some(r) = value.get("range") {
                hover["range"] = range(r)?;
            }
            json!({"kind":"hover","hover":hover})
        }
    } else {
        let items = if value.is_null() {
            vec![]
        } else if let Some(items) = value.as_array() {
            items.clone()
        } else {
            vec![value]
        };
        let mut output = Vec::new();
        for item in items.iter().take(max) {
            let (uri, r) = if item.get("targetUri").is_some() {
                (&item["targetUri"], &item["targetSelectionRange"])
            } else {
                (&item["uri"], &item["range"])
            };
            let uri = uri.as_str().ok_or("invalid LSP location URI")?;
            Url::parse(uri).map_err(|_| "invalid LSP location URI")?;
            output.push(json!({"uri":uri,"range":range(r)?}));
        }
        json!({"kind":"locations","locations":output,"resolvedWorkspaceUri":root,"truncated":items.len()>max})
    };
    loop {
        let text = serde_json::to_string(&result).map_err(|e| e.to_string())?;
        if text.chars().count() <= chars {
            return Ok(text);
        }
        result["truncated"] = json!(true);
        if let Some(items) = result["locations"].as_array_mut() {
            if items.pop().is_none() {
                return Err("LSP result metadata exceeds limit".into());
            }
        } else if let Some(content) = result.pointer("/hover/contents").and_then(Value::as_str) {
            let n = content.chars().count();
            if n == 0 {
                return Err("LSP result metadata exceeds limit".into());
            }
            result["hover"]["contents"] = json!(content.chars().take(n / 2).collect::<String>());
        } else {
            return Err("LSP result exceeds limit".into());
        }
    }
}
