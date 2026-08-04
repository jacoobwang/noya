use super::{
    Tool,
    filesystem::{SearchText, workspace_path},
};
use anyhow::{Context, Result, bail, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_RESULTS: usize = 100;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;

pub(super) struct CodeNavigation {
    pub(super) workspace: PathBuf,
    servers: Arc<Mutex<HashMap<String, Server>>>,
}

impl CodeNavigation {
    pub(super) fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            servers: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct Server {
    process: LspProcess,
    next_id: u64,
}

struct LspProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Drop for LspProcess {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Definition,
    References,
    WorkspaceSymbols,
}

impl Operation {
    fn parse(value: &str) -> Result<Self> {
        match value {
            "definition" => Ok(Self::Definition),
            "references" => Ok(Self::References),
            "workspace_symbols" => Ok(Self::WorkspaceSymbols),
            _ => bail!("operation must be definition, references, or workspace_symbols"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::WorkspaceSymbols => "workspace_symbols",
        }
    }
}

#[async_trait]
impl Tool for CodeNavigation {
    fn name(&self) -> &str {
        "code_navigation"
    }

    fn description(&self) -> &str {
        "Navigate code semantically with a local language server: find definitions, references, or workspace symbols"
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operation": {"type": "string", "enum": ["definition", "references", "workspace_symbols"]},
                "path": {"type": "string", "description": "Workspace-relative file path for definition or references"},
                "line": {"type": "integer", "minimum": 0, "description": "Zero-based line number"},
                "character": {"type": "integer", "minimum": 0, "description": "Zero-based UTF-16 character offset"},
                "query": {"type": "string", "description": "Symbol query for workspace_symbols"}
            },
            "required": ["operation"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: Value) -> Result<Value> {
        let operation = Operation::parse(
            args["operation"]
                .as_str()
                .context("operation must be a string")?,
        )?;
        if operation == Operation::WorkspaceSymbols {
            let query = args["query"].as_str().context("query must be a string")?;
            ensure!(!query.trim().is_empty(), "query cannot be empty");
            return self.workspace_symbols(query).await;
        }

        let raw_path = args["path"].as_str().context("path must be a string")?;
        let path = workspace_path(&self.workspace, raw_path)?;
        let line = args["line"]
            .as_u64()
            .context("line must be a non-negative integer")? as u32;
        let character = args["character"]
            .as_u64()
            .context("character must be a non-negative integer")? as u32;
        let language =
            language_for_path(&path).context("no supported language server for this file type")?;
        let content = tokio::fs::read_to_string(&path)
            .await
            .context("read source file for language server")?;
        let mut servers = self.servers.lock().await;
        let server = match self.ensure_server(&mut servers, language).await {
            Ok(server) => server,
            Err(error) => return Err(error),
        };
        let uri = path_to_uri(&path);
        server.notify("textDocument/didOpen", json!({
            "textDocument": {"uri": uri, "languageId": language, "version": 1, "text": content}
        })).await?;
        let method = match operation {
            Operation::Definition => "textDocument/definition",
            Operation::References => "textDocument/references",
            Operation::WorkspaceSymbols => unreachable!(),
        };
        let params = if operation == Operation::References {
            json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}, "context": {"includeDeclaration": true}})
        } else {
            json!({"textDocument": {"uri": uri}, "position": {"line": line, "character": character}})
        };
        let result = match server.request(method, params).await {
            Ok(result) => result,
            Err(error) => {
                servers.remove(language);
                return Err(error);
            }
        };
        Ok(location_result(&self.workspace, operation, result).await)
    }
}

impl CodeNavigation {
    async fn workspace_symbols(&self, query: &str) -> Result<Value> {
        let languages = languages_for_workspace(&self.workspace);
        if languages.is_empty() {
            return self
                .workspace_symbols_fallback(
                    query,
                    "no supported language server found in this workspace",
                )
                .await;
        }
        let mut servers = self.servers.lock().await;
        let mut results = Vec::new();
        let mut errors = Vec::new();
        for language in languages {
            let Ok(server) = self.ensure_server(&mut servers, language).await else {
                errors.push(format!("{language} server unavailable"));
                continue;
            };
            match server
                .request("workspace/symbol", json!({"query": query}))
                .await
            {
                Ok(result) => {
                    for symbol in result.as_array().cloned().unwrap_or_default() {
                        let Some(name) = symbol.get("name").and_then(Value::as_str) else {
                            continue;
                        };
                        let location = symbol.get("location").cloned().unwrap_or(Value::Null);
                        if let Some(mut item) = location_to_result(&self.workspace, &location).await
                        {
                            item["name"] = json!(name);
                            results.push(item);
                        }
                    }
                }
                Err(error) => {
                    servers.remove(language);
                    errors.push(error.to_string());
                }
            }
        }
        drop(servers);
        if !results.is_empty() {
            return Ok(result_json(Operation::WorkspaceSymbols, results));
        }
        self.workspace_symbols_fallback(query, &errors.join("; "))
            .await
    }

    async fn workspace_symbols_fallback(&self, query: &str, lsp_error: &str) -> Result<Value> {
        let fallback = SearchText {
            workspace: self.workspace.clone(),
        }
        .execute(json!({"query": query}))
        .await?;
        let matches = fallback
            .get("matches")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let mut results = Vec::new();
        let mut bytes = 0;
        for line in matches.lines() {
            let mut parts = line.splitn(3, ':');
            let Some(path) = parts.next() else { continue };
            let Some(line_number) = parts.next().and_then(|value| value.parse::<usize>().ok())
            else {
                continue;
            };
            let snippet = parts.next().unwrap_or_default();
            let result =
                json!({"path": path, "line": line_number, "character": 1, "snippet": snippet});
            let size = serde_json::to_vec(&result).map_or(0, |value| value.len());
            if results.len() >= MAX_RESULTS
                || (!results.is_empty() && bytes + size > MAX_OUTPUT_BYTES)
            {
                break;
            }
            bytes += size;
            results.push(result);
        }
        Ok(json!({
            "operation": Operation::WorkspaceSymbols.as_str(),
            "source": "rg-fallback",
            "results": results,
            "total": Value::Null,
            "returned": results.len(),
            "truncated": matches.lines().count() > results.len(),
            "lsp_error": lsp_error,
        }))
    }

    async fn ensure_server<'a>(
        &self,
        servers: &'a mut HashMap<String, Server>,
        language: &str,
    ) -> Result<&'a mut Server> {
        if servers
            .get_mut(language)
            .is_some_and(|server| server.process.child.try_wait().ok().flatten().is_some())
        {
            servers.remove(language);
        }
        if !servers.contains_key(language) {
            let command = server_command(language)?;
            let mut process = spawn_server(&self.workspace, &command).await?;
            initialize(&mut process, &self.workspace).await?;
            servers.insert(
                language.to_string(),
                Server {
                    process,
                    next_id: 1,
                },
            );
        }
        Ok(servers
            .get_mut(language)
            .expect("server inserted or already present"))
    }
}

impl Server {
    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        write_message(
            &mut self.process.stdin,
            &json!({"jsonrpc":"2.0", "method": method, "params": params}),
        )
        .await
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        write_message(
            &mut self.process.stdin,
            &json!({"jsonrpc":"2.0", "id": id, "method": method, "params": params}),
        )
        .await?;
        let response = timeout(
            REQUEST_TIMEOUT,
            read_response_for_id(&mut self.process.stdout, id),
        )
        .await
        .context("language server request timed out")??;
        if let Some(error) = response.get("error") {
            bail!("language server returned an error: {error}");
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }
}

async fn spawn_server(workspace: &Path, command: &ServerCommand) -> Result<LspProcess> {
    let mut child = Command::new(&command.program)
        .args(&command.args)
        .current_dir(workspace)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("start language server `{}`", command.program))?;
    Ok(LspProcess {
        stdin: child
            .stdin
            .take()
            .context("language server stdin unavailable")?,
        stdout: BufReader::new(
            child
                .stdout
                .take()
                .context("language server stdout unavailable")?,
        ),
        child,
    })
}

async fn initialize(process: &mut LspProcess, workspace: &Path) -> Result<()> {
    let params = json!({
        "processId": std::process::id(),
        "rootUri": path_to_uri(workspace),
        "capabilities": {},
        "workspaceFolders": [{"uri": path_to_uri(workspace), "name": workspace.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")}]
    });
    write_message(
        &mut process.stdin,
        &json!({"jsonrpc":"2.0", "id": 0, "method":"initialize", "params": params}),
    )
    .await?;
    let response = timeout(
        INITIALIZE_TIMEOUT,
        read_response_for_id(&mut process.stdout, 0),
    )
    .await
    .context("language server initialization timed out")??;
    if let Some(error) = response.get("error") {
        bail!("language server initialization failed: {error}");
    }
    write_message(
        &mut process.stdin,
        &json!({"jsonrpc":"2.0", "method":"initialized", "params": {}}),
    )
    .await
}

async fn write_message(writer: &mut ChildStdin, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    writer
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    writer.write_all(&body).await?;
    writer
        .flush()
        .await
        .context("flush language server request")
}

async fn read_response(reader: &mut BufReader<ChildStdout>) -> Result<Value> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line).await?;
        ensure!(read > 0, "language server closed stdout");
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .context("invalid language server content length")?,
            );
        }
    }
    let length = content_length.context("language server response omitted Content-Length")?;
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body).context("decode language server response")?)
}

async fn read_response_for_id(
    reader: &mut BufReader<ChildStdout>,
    expected_id: u64,
) -> Result<Value> {
    loop {
        let response = read_response(reader).await?;
        if response.get("id").and_then(Value::as_u64) == Some(expected_id) {
            return Ok(response);
        }
    }
}

struct ServerCommand {
    program: String,
    args: Vec<String>,
}

fn server_command(language: &str) -> Result<ServerCommand> {
    let env_name = format!("NOYA_LSP_{}", language.to_ascii_uppercase());
    let default = match language {
        "rust" => ("rust-analyzer", vec![]),
        "cpp" => ("clangd", vec!["--log=error"]),
        "go" => ("gopls", vec!["serve"]),
        "python" => ("pyright-langserver", vec!["--stdio"]),
        "typescript" => ("typescript-language-server", vec!["--stdio"]),
        "lua" => ("lua-language-server", vec!["--stdio"]),
        "bash" => ("bash-language-server", vec!["start"]),
        "java" => ("jdtls", vec![]),
        _ => bail!("unsupported language: {language}"),
    };
    let command = std::env::var(&env_name).ok();
    let (program, args) = if let Some(command) = command {
        let mut parts = command.split_whitespace().map(str::to_string);
        let program = parts
            .next()
            .context("LSP override command cannot be empty")?;
        (program, parts.collect())
    } else {
        (
            default.0.to_string(),
            default.1.into_iter().map(str::to_string).collect(),
        )
    };
    Ok(ServerCommand { program, args })
}

fn language_for_path(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Some("rust"),
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" => Some("cpp"),
        "go" => Some("go"),
        "py" => Some("python"),
        "ts" | "tsx" | "js" | "jsx" => Some("typescript"),
        "lua" => Some("lua"),
        "sh" | "bash" => Some("bash"),
        "java" => Some("java"),
        _ => None,
    }
}

fn languages_for_workspace(workspace: &Path) -> Vec<&'static str> {
    let mut languages = Vec::new();
    for entry in walk_files(workspace) {
        if let Some(language) = language_for_path(&entry) {
            if !languages.contains(&language) {
                languages.push(language);
            }
        }
    }
    languages
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return files;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().and_then(|name| name.to_str()) == Some(".git") {
            continue;
        }
        if path.is_dir() {
            files.extend(walk_files(&path));
        } else {
            files.push(path);
        }
    }
    files
}

async fn location_result(workspace: &Path, operation: Operation, result: Value) -> Value {
    let locations = result.as_array().cloned().unwrap_or_default();
    let mut results = Vec::new();
    for location in locations {
        if let Some(item) = location_to_result(workspace, &location).await {
            results.push(item);
        }
    }
    result_json(operation, results)
}

async fn location_to_result(workspace: &Path, location: &Value) -> Option<Value> {
    let uri = location
        .get("uri")
        .or_else(|| location.get("targetUri"))?
        .as_str()?;
    let path = uri_to_path(uri)?;
    if !path.starts_with(workspace) {
        return None;
    }
    let range = location
        .get("range")
        .or_else(|| location.get("targetSelectionRange"))?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as usize;
    let character = start.get("character")?.as_u64()? as usize;
    let snippet = tokio::fs::read_to_string(&path)
        .await
        .ok()
        .and_then(|content| content.lines().nth(line).map(str::to_string));
    Some(json!({
        "path": path.strip_prefix(workspace).unwrap_or(&path),
        "line": line + 1,
        "character": character + 1,
        "snippet": snippet,
    }))
}

fn result_json(operation: Operation, mut results: Vec<Value>) -> Value {
    results.sort_by_key(|result| {
        (
            result["path"].as_str().unwrap_or_default().to_string(),
            result["line"].as_u64().unwrap_or(0),
        )
    });
    let total = results.len();
    let mut returned = Vec::new();
    let mut bytes = 0;
    for result in results.into_iter().take(MAX_RESULTS) {
        let size = serde_json::to_vec(&result).map_or(0, |value| value.len());
        if !returned.is_empty() && bytes + size > MAX_OUTPUT_BYTES {
            break;
        }
        bytes += size;
        returned.push(result);
    }
    json!({"operation": operation.as_str(), "source": "lsp", "results": returned, "total": total, "returned": returned.len(), "truncated": returned.len() < total})
}

fn path_to_uri(path: &Path) -> String {
    format!("file://{}", path.display())
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let path = uri
        .strip_prefix("file://")?
        .replace("%20", " ")
        .replace("%23", "#");
    Some(PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_supported_extensions_to_languages() {
        assert_eq!(language_for_path(Path::new("src/main.rs")), Some("rust"));
        assert_eq!(
            language_for_path(Path::new("src/main.ts")),
            Some("typescript")
        );
        assert_eq!(language_for_path(Path::new("src/Main.java")), Some("java"));
        assert_eq!(language_for_path(Path::new("README.md")), None);
    }

    #[test]
    fn result_is_bounded_and_sorted() {
        let result = result_json(
            Operation::Definition,
            (0..150)
                .rev()
                .map(|line| json!({"path": format!("z/{line}"), "line": line}))
                .collect(),
        );
        assert_eq!(result["returned"], 100);
        assert_eq!(result["truncated"], true);
    }
}
