use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc, Mutex,
};
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use uuid::Uuid;

const MAX_SPAWN_LINE_BUFFER_CHARS: usize = 16 * 1024;
const MAX_SPAWN_TASK_CHARS: usize = 2048;
const MAIN_SESSION_ID: &str = "active-workspace-session";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    pub session_id: String,
    pub run_id: Option<String>,
    pub model: String, // "claude", "gemini", "grok", "codex"
    pub specific_model: Option<String>,
    pub prompt: String,
    pub agent_mappings: Option<HashMap<String, String>>,
    pub provider_models: Option<HashMap<String, String>>,
    pub subagent_models: Option<HashMap<String, AgentModelSelection>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentModelSelection {
    pub name: String,
    pub role: String,
    pub provider: String,
    pub specific_model: String,
    pub active: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutputEvent {
    pub session_id: String,
    pub stream: String, // "stdout" or "stderr"
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliFinishedEvent {
    pub session_id: String,
    pub success: bool,
    pub exit_code: Option<i32>,
    pub message: String,
}

pub struct CliSession {
    pub child: Arc<Mutex<Child>>,
    pub finish_on_exit: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct CliMediator {
    pub active_sessions: Arc<Mutex<HashMap<String, CliSession>>>,
}

impl CliMediator {
    pub fn new() -> Self {
        Self {
            active_sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn send_prompt(
        &self,
        app: AppHandle,
        req: PromptRequest,
        workspace_root: std::path::PathBuf,
    ) -> Result<(), String> {
        let req = validate_prompt_request(req)?;
        let session_id = req.session_id.clone();
        let run_id = req
            .run_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let routed_agent_key = route_agent_key(&req.prompt);
        let final_model = route_task(&req.prompt, &req.model, req.agent_mappings.as_ref());
        let mut specific_model =
            select_specific_model(&req, &final_model, routed_agent_key.as_deref());

        if final_model == "claude" {
            if let Some(ref m) = specific_model {
                specific_model = Some(m.replace(".", "-"));
            }
        }

        let cleaned_prompt = clean_prompt(&req.prompt);
        let allow_subagent_spawns = session_allows_subagent_spawns(&session_id);
        let prompt_with_harness = if allow_subagent_spawns {
            let agent_registry = format_agent_registry(
                req.agent_mappings.as_ref(),
                req.provider_models.as_ref(),
                req.subagent_models.as_ref(),
            );
            let delegation_policy = if is_casual_chat_prompt(&cleaned_prompt) {
                "Delegation mode: casual chat.\n- This prompt appears to be a normal conversation. You may answer directly without spawning subagents.\n- If you realize the answer requires research, file inspection, code changes, tests, command execution, or other project work, you MUST delegate to the relevant canonical agents before proceeding."
            } else {
                "Delegation mode: mandatory delegation.\n- This prompt is non-casual work. You MUST involve the relevant companion agents by printing SPAWN_SUBAGENT lines for the canonical roles that should own parts of the task.\n- Delegate research/inspection to research, implementation/backend/filesystem work to backend, UI/app styling work to frontend, and checks/tests/validation to verification.\n- Your coordinator response should stay concise and should not replace the assigned workers' slices."
            };
            format!(
                "{}\n\n[SYSTEM INSTRUCTION]: You are running in the nTropy workspace environment. Current backend execution target: provider={} specific_model={}. Available companion subagents are:\n{}\n\nStrict delegation protocol:\n- You are the only nTropy coordinator for this user request.\n- {}\n- Delegate work by printing one line per delegated task in exactly this format, with no commas inside values:\nSPAWN_SUBAGENT:agent=<research|backend|frontend|verification>,task=<specific task>\n- You MUST use only these exact canonical agent keys: research, backend, frontend, verification.\n- Do not invent agent names, alternate labels, provider names, or underlying model names in delegation commands.\n- The nTropy backend will ignore invented labels and resolve the user's selected provider/model from the registry for that exact agent key.",
                cleaned_prompt,
                final_model,
                specific_model.as_deref().unwrap_or("provider-default"),
                agent_registry,
                delegation_policy
            )
        } else {
            format!(
                "{}\n\n[SYSTEM INSTRUCTION]: You are running in the nTropy workspace environment. Current backend execution target: provider={} specific_model={}. This is a worker execution session, not the top-level nTropy coordinator.\n\nDelegation boundary:\n- Nested subagent delegation is disabled for this session.\n- Do not print SPAWN_SUBAGENT commands.\n- Work only on the assigned task and return concise final output for the shared chat.",
                cleaned_prompt,
                final_model,
                specific_model.as_deref().unwrap_or("provider-default")
            )
        };

        // Terminate any existing running process for this session first
        {
            let mut sessions = self.active_sessions.lock().unwrap();
            if let Some(old_session) = sessions.remove(&session_id) {
                old_session.finish_on_exit.store(false, Ordering::SeqCst);
                println!(
                    "Killing previous running process for session '{}'",
                    session_id
                );
                if let Err(e) = terminate_child_process(&old_session.child, &session_id) {
                    println!("Warning: Failed to terminate previous running process for session '{}' (process may have already terminated): {}", session_id, e);
                }
            }
        }

        // Print visual confirmation of orchestrator routing and folder harness to user stdout stream
        let display_model_info = if let Some(ref m) = specific_model {
            format!("{} ({})", final_model.to_uppercase(), m)
        } else {
            format!("{} (provider-default)", final_model.to_uppercase())
        };

        let _ = app.emit("cli-output", ProcessOutputEvent {
            session_id: session_id.clone(),
            stream: "stdout".to_string(),
            data: format!(
                "[ORCHESTRATOR] 🎯 Routing task automatically to: {} CLI based on intent.\n[ORCHESTRATOR] 📂 Harness Active Directory: {}\n\n",
                display_model_info,
                workspace_root.to_string_lossy()
            ),
        });

        let _ = app.emit(
            "cli-output",
            ProcessOutputEvent {
                session_id: session_id.clone(),
                stream: "stdout".to_string(),
                data: format!(
                "[ORCHESTRATOR] Exact backend model selection: provider={} specific_model={}\n\n",
                final_model,
                specific_model.as_deref().unwrap_or("provider-default")
            ),
            },
        );

        println!(
            "Spawning new CLI process for model '{}' in session '{}' inside directory {:?}",
            final_model, session_id, workspace_root
        );

        let mut child = spawn_provider_cli(
            &final_model,
            specific_model.as_deref(),
            &prompt_with_harness,
            &workspace_root,
        )?;

        // If stdin is piped, write the prompt to it and immediately close it
        if let Some(mut stdin) = child.stdin.take() {
            if final_model == "claude" || final_model == "codex" {
                let mut prompt_bytes = prompt_with_harness.as_bytes().to_vec();
                prompt_bytes.push(b'\n');
                let _ = stdin.write_all(&prompt_bytes);
                let _ = stdin.flush();
            }
            // Explicitly drop stdin to close it, signaling EOF so process executes immediately
            std::mem::drop(stdin);
        }

        let stdout = child.stdout.take().ok_or("Failed to capture stdout")?;
        let stderr = child.stderr.take().ok_or("Failed to capture stderr")?;
        let child_handle = Arc::new(Mutex::new(child));
        let finish_on_exit = Arc::new(AtomicBool::new(true));
        let (stdout_done_tx, stdout_done_rx) = mpsc::channel();
        let (stderr_done_tx, stderr_done_rx) = mpsc::channel();

        // Spawn background threads to continuously read stdout and stderr chunk-by-chunk in real-time
        let app_stdout = app.clone();
        let session_id_stdout = session_id.clone();
        let run_id_stdout = run_id.clone();
        std::thread::spawn(move || {
            let mut reader = stdout;
            let mut buf = [0u8; 1024];
            let mut line_buffer = String::new();

            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let chunk_str = String::from_utf8_lossy(&buf[..n]).to_string();

                        let _ = app_stdout.emit(
                            "cli-output",
                            ProcessOutputEvent {
                                session_id: session_id_stdout.clone(),
                                stream: "stdout".to_string(),
                                data: chunk_str.clone(),
                            },
                        );

                        // Accumulate line_buffer for SPAWN_SUBAGENT detection
                        line_buffer.push_str(&chunk_str);
                        if line_buffer.len() > MAX_SPAWN_LINE_BUFFER_CHARS {
                            let char_count = line_buffer.chars().count();
                            line_buffer = line_buffer
                                .chars()
                                .skip(char_count.saturating_sub(MAX_SPAWN_LINE_BUFFER_CHARS))
                                .collect();
                            if let Some(pos) = line_buffer.find('\n') {
                                line_buffer = line_buffer[pos + 1..].to_string();
                            }
                        }
                        while let Some(pos) = line_buffer.find('\n') {
                            let line = line_buffer[..pos].trim_end().to_string();
                            line_buffer = line_buffer[pos + 1..].to_string();

                            if let Some(spawn_req) =
                                parse_spawn_command(&line, &session_id_stdout, &run_id_stdout)
                            {
                                if session_allows_subagent_spawns(&session_id_stdout) {
                                    println!(
                                        "Dynamic Subagent Spawn Request intercepted: {:?}",
                                        spawn_req
                                    );
                                    let _ = app_stdout.emit("spawn-subagent", spawn_req);
                                } else {
                                    let _ = app_stdout.emit(
                                        "cli-output",
                                        ProcessOutputEvent {
                                            session_id: session_id_stdout.clone(),
                                            stream: "stdout".to_string(),
                                            data: format!(
                                                "[ORCHESTRATOR] Ignored nested SPAWN_SUBAGENT from {}; only the main orchestrator session may spawn subagents.\n",
                                                session_id_stdout
                                            ),
                                        },
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!(
                            "[ERROR] Failed to read stdout stream chunk for session '{}': {}",
                            session_id_stdout, e
                        );
                        break;
                    }
                }
            }

            if !line_buffer.is_empty() {
                let line = line_buffer.trim_end().to_string();
                if let Some(spawn_req) =
                    parse_spawn_command(&line, &session_id_stdout, &run_id_stdout)
                {
                    if session_allows_subagent_spawns(&session_id_stdout) {
                        println!(
                            "Dynamic Subagent Spawn Request intercepted: {:?}",
                            spawn_req
                        );
                        let _ = app_stdout.emit("spawn-subagent", spawn_req);
                    } else {
                        let _ = app_stdout.emit(
                            "cli-output",
                            ProcessOutputEvent {
                                session_id: session_id_stdout.clone(),
                                stream: "stdout".to_string(),
                                data: format!(
                                    "[ORCHESTRATOR] Ignored nested SPAWN_SUBAGENT from {}; only the main orchestrator session may spawn subagents.\n",
                                    session_id_stdout
                                ),
                            },
                        );
                    }
                }
            }

            let _ = stdout_done_tx.send(());
        });

        let app_stderr = app.clone();
        let session_id_stderr = session_id.clone();
        std::thread::spawn(move || {
            let mut reader = stderr;
            let mut buf = [0u8; 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let chunk_str = String::from_utf8_lossy(&buf[..n]).to_string();
                        let _ = app_stderr.emit(
                            "cli-output",
                            ProcessOutputEvent {
                                session_id: session_id_stderr.clone(),
                                stream: "stderr".to_string(),
                                data: chunk_str,
                            },
                        );
                    }
                    Err(e) => {
                        eprintln!(
                            "[ERROR] Failed to read stderr stream chunk for session '{}': {}",
                            session_id_stderr, e
                        );
                        break;
                    }
                }
            }
            let _ = stderr_done_tx.send(());
        });

        {
            let mut sessions = self.active_sessions.lock().unwrap();
            sessions.insert(
                session_id.clone(),
                CliSession {
                    child: child_handle.clone(),
                    finish_on_exit: finish_on_exit.clone(),
                },
            );
        }

        let app_finish = app.clone();
        let session_id_finish = session_id.clone();
        let active_sessions = self.active_sessions.clone();
        std::thread::spawn(move || {
            monitor_child_process(
                app_finish,
                session_id_finish,
                child_handle,
                active_sessions,
                finish_on_exit,
                stdout_done_rx,
                stderr_done_rx,
            );
        });

        Ok(())
    }

    pub fn terminate_session(&self, session_id: &str) -> Result<(), String> {
        let mut sessions = self.active_sessions.lock().unwrap();
        if let Some(session) = sessions.remove(session_id) {
            println!("Terminating CLI session '{}'", session_id);
            terminate_child_process(&session.child, session_id)
        } else {
            Err(format!("No active CLI session found for '{}'", session_id))
        }
    }
}

#[derive(Debug, Clone)]
struct CliLaunchCandidate {
    program: OsString,
    pre_args: Vec<OsString>,
    label: String,
}

impl CliLaunchCandidate {
    fn command(command: &str) -> Self {
        Self {
            program: OsString::from(command),
            pre_args: Vec::new(),
            label: command.to_string(),
        }
    }

    fn program_path(path: PathBuf) -> Self {
        let label = path.to_string_lossy().to_string();
        Self {
            program: path.into_os_string(),
            pre_args: Vec::new(),
            label,
        }
    }

    fn node_script(script_path: PathBuf) -> Self {
        let label = format!("node {}", script_path.to_string_lossy());
        Self {
            program: OsString::from("node"),
            pre_args: vec![script_path.into_os_string()],
            label,
        }
    }
}

fn spawn_provider_cli(
    provider: &str,
    specific_model: Option<&str>,
    prompt_with_harness: &str,
    workspace_root: &Path,
) -> Result<Child, String> {
    let candidates = cli_launch_candidates(provider)?;
    let path_env = augmented_path_env();
    let mut attempts = Vec::new();

    for candidate in candidates {
        let mut cmd = Command::new(&candidate.program);
        cmd.current_dir(workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(ref path) = path_env {
            cmd.env("PATH", path);
        }
        cmd.args(&candidate.pre_args);
        apply_model_env(&mut cmd, specific_model);
        apply_provider_args(&mut cmd, provider, specific_model, prompt_with_harness)?;

        match cmd.spawn() {
            Ok(child) => {
                println!(
                    "Spawned {} CLI using {} on {}.",
                    provider,
                    candidate.label,
                    std::env::consts::OS
                );
                return Ok(child);
            }
            Err(err) => attempts.push(format!("{} ({})", candidate.label, err)),
        }
    }

    let provider_env = format!("NTROPY_{}_BIN", provider.to_ascii_uppercase());
    let gemini_hint = if provider == "gemini" {
        " or NTROPY_GEMINI_JS"
    } else {
        ""
    };
    Err(format!(
        "Failed to spawn {} CLI on {}. Tried: {}. Install the CLI on PATH or set {}{}.",
        provider,
        std::env::consts::OS,
        attempts.join("; "),
        provider_env,
        gemini_hint
    ))
}

fn apply_model_env(cmd: &mut Command, specific_model: Option<&str>) {
    if let Some(model) = specific_model {
        cmd.env("CLAUDE_MODEL", model);
        cmd.env("GEMINI_MODEL", model);
        cmd.env("GROK_MODEL", model);
        cmd.env("OPENAI_MODEL", model);
        cmd.env("LLM_MODEL", model);
    }
}

fn apply_provider_args(
    cmd: &mut Command,
    provider: &str,
    specific_model: Option<&str>,
    prompt_with_harness: &str,
) -> Result<(), String> {
    match provider {
        "claude" => {
            cmd.args([
                "--print",
                "--dangerously-skip-permissions",
                "--permission-mode",
                "bypassPermissions",
            ]);
            if let Some(model) = specific_model {
                cmd.args(["--model", model]);
            }
        }
        "grok" => {
            cmd.args(["--always-approve", "--permission-mode", "bypassPermissions"]);
            if let Some(model) = specific_model {
                cmd.args(["-m", model]);
            }
            cmd.args(["-p", prompt_with_harness]);
        }
        "codex" => {
            cmd.args([
                "exec",
                "--ignore-user-config",
                "--disable",
                "plugins",
                "--disable",
                "remote_plugin",
                "--disable",
                "shell_snapshot",
                "-c",
                "model_reasoning_effort=\"xhigh\"",
            ]);
            if let Some(model) = specific_model {
                cmd.args(["--model", model]);
            }
            cmd.args([
                "--skip-git-repo-check",
                "--dangerously-bypass-approvals-and-sandbox",
                "-",
            ]);
        }
        "gemini" => {
            cmd.args(["--skip-trust", "--approval-mode", "yolo"]);
            if let Some(model) = specific_model {
                cmd.args(["--model", model]);
            }
            cmd.args(["--prompt", prompt_with_harness]);
        }
        _ => return Err(format!("Unsupported model/CLI: {}", provider)),
    }
    Ok(())
}

fn cli_launch_candidates(provider: &str) -> Result<Vec<CliLaunchCandidate>, String> {
    if !is_supported_provider(provider) {
        return Err(format!("Unsupported model/CLI: {}", provider));
    }

    let mut candidates = Vec::new();
    push_env_candidate(&mut candidates, provider);

    if provider == "gemini" {
        if let Some(script) = non_empty_env_path("NTROPY_GEMINI_JS") {
            candidates.push(CliLaunchCandidate::node_script(script));
        }
    }

    push_platform_candidates(&mut candidates, provider);
    push_path_candidates(&mut candidates, provider);

    Ok(candidates)
}

fn push_env_candidate(candidates: &mut Vec<CliLaunchCandidate>, provider: &str) {
    let env_name = format!("NTROPY_{}_BIN", provider.to_ascii_uppercase());
    if let Some(path) = non_empty_env_path(&env_name) {
        candidates.push(CliLaunchCandidate::program_path(path));
    }
}

fn push_platform_candidates(candidates: &mut Vec<CliLaunchCandidate>, provider: &str) {
    if cfg!(target_os = "windows") {
        if let Some(appdata) = non_empty_env_path("APPDATA") {
            match provider {
                "claude" => push_existing_program(
                    candidates,
                    appdata
                        .join("npm")
                        .join("node_modules")
                        .join("@anthropic-ai")
                        .join("claude-code")
                        .join("bin")
                        .join("claude.exe"),
                ),
                "codex" => push_existing_program(
                    candidates,
                    appdata
                        .join("npm")
                        .join("node_modules")
                        .join("@openai")
                        .join("codex")
                        .join("node_modules")
                        .join("@openai")
                        .join("codex-win32-x64")
                        .join("vendor")
                        .join("x86_64-pc-windows-msvc")
                        .join("codex")
                        .join("codex.exe"),
                ),
                "gemini" => push_existing_node_script(
                    candidates,
                    appdata
                        .join("npm")
                        .join("node_modules")
                        .join("@google")
                        .join("gemini-cli")
                        .join("bundle")
                        .join("gemini.js"),
                ),
                _ => {}
            }
        }

        if provider == "grok" {
            if let Some(profile) = non_empty_env_path("USERPROFILE") {
                push_existing_program(
                    candidates,
                    profile.join(".grok").join("bin").join("grok.exe"),
                );
            }
        }
        return;
    }

    if let Some(home) = home_dir() {
        match provider {
            "grok" => {
                push_existing_program(candidates, home.join(".grok").join("bin").join("grok"))
            }
            "gemini" => {
                for root in npm_global_roots(&home) {
                    push_existing_node_script(
                        candidates,
                        root.join("lib")
                            .join("node_modules")
                            .join("@google")
                            .join("gemini-cli")
                            .join("bundle")
                            .join("gemini.js"),
                    );
                }
            }
            _ => {}
        }
    }
}

fn push_path_candidates(candidates: &mut Vec<CliLaunchCandidate>, provider: &str) {
    candidates.push(CliLaunchCandidate::command(provider));

    if cfg!(target_os = "windows") {
        candidates.push(CliLaunchCandidate::command(&format!("{}.exe", provider)));
        candidates.push(CliLaunchCandidate::command(&format!("{}.cmd", provider)));
    }
}

fn push_existing_program(candidates: &mut Vec<CliLaunchCandidate>, path: PathBuf) {
    if path.exists() {
        candidates.push(CliLaunchCandidate::program_path(path));
    }
}

fn push_existing_node_script(candidates: &mut Vec<CliLaunchCandidate>, script_path: PathBuf) {
    if script_path.exists() {
        candidates.push(CliLaunchCandidate::node_script(script_path));
    }
}

fn non_empty_env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

fn npm_global_roots(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".npm-global")];
    if cfg!(target_os = "macos") {
        roots.push(PathBuf::from("/opt/homebrew"));
        roots.push(PathBuf::from("/usr/local"));
    } else {
        roots.push(PathBuf::from("/usr/local"));
    }
    roots
}

fn augmented_path_env() -> Option<OsString> {
    let mut paths = common_bin_dirs()
        .into_iter()
        .filter(|path| path.exists())
        .collect::<Vec<_>>();

    if let Some(existing_path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&existing_path));
    }

    std::env::join_paths(paths).ok()
}

fn common_bin_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(appdata) = non_empty_env_path("APPDATA") {
        dirs.push(appdata.join("npm"));
    }
    if let Some(profile) = non_empty_env_path("USERPROFILE") {
        dirs.push(profile.join(".grok").join("bin"));
        dirs.push(profile.join(".cargo").join("bin"));
        dirs.push(profile.join(".local").join("bin"));
    }
    if let Some(home) = home_dir() {
        dirs.push(home.join(".grok").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".npm-global").join("bin"));
        dirs.push(home.join(".bun").join("bin"));
    }

    if cfg!(target_os = "macos") {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/bin"));
    } else if cfg!(target_os = "linux") {
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/bin"));
        dirs.push(PathBuf::from("/bin"));
    }

    dirs
}

fn monitor_child_process(
    app: AppHandle,
    session_id: String,
    child_handle: Arc<Mutex<Child>>,
    active_sessions: Arc<Mutex<HashMap<String, CliSession>>>,
    finish_on_exit: Arc<AtomicBool>,
    stdout_done_rx: mpsc::Receiver<()>,
    stderr_done_rx: mpsc::Receiver<()>,
) {
    let finished = loop {
        let wait_result = {
            let mut child = match child_handle.lock() {
                Ok(child) => child,
                Err(_) => {
                    break CliFinishedEvent {
                        session_id: session_id.clone(),
                        success: false,
                        exit_code: None,
                        message:
                            "Failed to observe CLI exit status: child process lock was poisoned"
                                .to_string(),
                    };
                }
            };
            child.try_wait()
        };

        match wait_result {
            Ok(Some(status)) => break cli_finished_from_status(session_id.clone(), status),
            Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            Err(e) => {
                break CliFinishedEvent {
                    session_id: session_id.clone(),
                    success: false,
                    exit_code: None,
                    message: format!("Failed to observe CLI exit status: {}", e),
                };
            }
        }
    };

    let _ = stdout_done_rx.recv_timeout(Duration::from_secs(2));
    let _ = stderr_done_rx.recv_timeout(Duration::from_secs(2));

    {
        let mut sessions = active_sessions.lock().unwrap();
        let should_remove = sessions
            .get(&session_id)
            .map(|session| Arc::ptr_eq(&session.child, &child_handle))
            .unwrap_or(false);
        if should_remove {
            sessions.remove(&session_id);
        }
    }

    if finish_on_exit.load(Ordering::SeqCst) {
        let _ = app.emit("cli-finished", finished);
    }
}

fn cli_finished_from_status(session_id: String, status: ExitStatus) -> CliFinishedEvent {
    let exit_code = status.code();
    let success = status.success();
    let message = if success {
        "CLI process exited successfully".to_string()
    } else if let Some(code) = exit_code {
        format!("CLI process exited with code {}", code)
    } else {
        "CLI process terminated without an exit code".to_string()
    };

    CliFinishedEvent {
        session_id,
        success,
        exit_code,
        message,
    }
}

fn terminate_child_process(
    child_handle: &Arc<Mutex<Child>>,
    session_id: &str,
) -> Result<(), String> {
    let mut child = child_handle.lock().map_err(|_| {
        format!(
            "Failed to terminate CLI session '{}': child process lock was poisoned",
            session_id
        )
    })?;

    match child.try_wait() {
        Ok(Some(_)) => Ok(()),
        Ok(None) => {
            if let Err(e) = child.kill() {
                if child.try_wait().ok().flatten().is_some() {
                    return Ok(());
                }
                return Err(format!(
                    "Failed to terminate CLI session '{}': {}",
                    session_id, e
                ));
            }
            child
                .wait()
                .map(|_| ())
                .map_err(|e| format!("Failed to reap CLI session '{}': {}", session_id, e))
        }
        Err(e) => Err(format!(
            "Failed to inspect CLI session '{}': {}",
            session_id, e
        )),
    }
}

fn validate_prompt_request(mut req: PromptRequest) -> Result<PromptRequest, String> {
    req.session_id = validate_session_id(&req.session_id)?;
    req.run_id = Some(match req.run_id.take() {
        Some(run_id) => validate_run_id(&run_id)?,
        None => Uuid::new_v4().to_string(),
    });
    req.model = normalize_provider(&req.model, "model")?;
    req.specific_model = match req.specific_model.take() {
        Some(model) => normalize_optional_model(&req.model, &model, "specific_model")?,
        None => None,
    };

    if let Some(agent_mappings) = req.agent_mappings.take() {
        let mut normalized = HashMap::new();
        for (agent_key, provider) in agent_mappings {
            let agent_key = normalize_agent_key(&agent_key, "agent_mappings")?;
            let provider = normalize_provider(&provider, &format!("agent_mappings.{}", agent_key))?;
            normalized.insert(agent_key, provider);
        }
        req.agent_mappings = Some(normalized);
    }

    if let Some(provider_models) = req.provider_models.take() {
        let mut normalized = HashMap::new();
        for (provider, model) in provider_models {
            let provider = normalize_provider(&provider, "provider_models")?;
            if let Some(model) = normalize_optional_model(
                &provider,
                &model,
                &format!("provider_models.{}", provider),
            )? {
                normalized.insert(provider, model);
            }
        }
        req.provider_models = Some(normalized);
    }

    if let Some(subagent_models) = req.subagent_models.take() {
        let mut normalized = HashMap::new();
        for (agent_key, mut selection) in subagent_models {
            let agent_key = normalize_agent_key(&agent_key, "subagent_models")?;
            selection.provider = normalize_provider(
                &selection.provider,
                &format!("subagent_models.{}.provider", agent_key),
            )?;
            let model_field = format!("subagent_models.{}.specific_model", agent_key);
            selection.specific_model = if selection
                .specific_model
                .trim()
                .eq_ignore_ascii_case("provider-default")
            {
                "provider-default".to_string()
            } else {
                normalize_required_model(
                    &selection.provider,
                    &selection.specific_model,
                    &model_field,
                )?
            };
            normalized.insert(agent_key, selection);
        }
        req.subagent_models = Some(normalized);
    }

    Ok(req)
}

fn validate_session_id(session_id: &str) -> Result<String, String> {
    let trimmed = session_id.trim();
    if trimmed.is_empty() {
        return Err("session_id cannot be empty".to_string());
    }
    if trimmed.len() > 128 {
        return Err("session_id is too long".to_string());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'))
    {
        return Err("session_id contains unsupported characters".to_string());
    }
    Ok(trimmed.to_string())
}

fn validate_run_id(run_id: &str) -> Result<String, String> {
    let trimmed = run_id.trim();
    if trimmed.is_empty() {
        return Err("run_id cannot be empty".to_string());
    }
    if trimmed.len() > 128 {
        return Err("run_id is too long".to_string());
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("run_id contains unsupported characters".to_string());
    }
    Ok(trimmed.to_string())
}

fn normalize_agent_key(agent_key: &str, field: &str) -> Result<String, String> {
    let normalized = agent_key.trim().to_lowercase();
    if is_canonical_agent_key(&normalized) {
        Ok(normalized)
    } else {
        Err(format!(
            "Unsupported {} agent key '{}'. Expected one of: research, backend, frontend, verification",
            field, agent_key
        ))
    }
}

fn normalize_provider(provider: &str, field: &str) -> Result<String, String> {
    let normalized = provider.trim().to_lowercase();
    if is_supported_provider(&normalized) {
        Ok(normalized)
    } else {
        Err(format!(
            "Unsupported provider selection for {}: '{}'. Expected one of: claude, gemini, grok, codex",
            field, provider
        ))
    }
}

fn normalize_optional_model(
    provider: &str,
    model: &str,
    field: &str,
) -> Result<Option<String>, String> {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("provider-default") {
        return Ok(None);
    }
    normalize_required_model(provider, trimmed, field).map(Some)
}

fn normalize_required_model(provider: &str, model: &str, field: &str) -> Result<String, String> {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("provider-default") {
        return Err(format!(
            "Model selection for {} cannot be empty or provider-default",
            field
        ));
    }
    if trimmed.len() > 128 || !trimmed.chars().all(is_safe_model_char) {
        return Err(format!(
            "Model selection for {} contains unsupported characters",
            field
        ));
    }
    if !is_known_model_for_provider(provider, trimmed) {
        return Err(format!(
            "Unsupported model selection for {}: provider={} model={}",
            field, provider, trimmed
        ));
    }
    Ok(trimmed.to_string())
}

fn is_safe_model_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '/' | '+')
}

fn is_supported_provider(provider: &str) -> bool {
    matches!(provider, "claude" | "gemini" | "grok" | "codex")
}

fn session_allows_subagent_spawns(session_id: &str) -> bool {
    session_id == MAIN_SESSION_ID
}

fn is_casual_chat_prompt(prompt: &str) -> bool {
    let lower = prompt.to_lowercase();
    let trimmed = lower.trim();
    if trimmed.is_empty() {
        return true;
    }

    let has_code_or_path_signal = trimmed.contains("```")
        || trimmed.contains("[stdout]")
        || trimmed.contains("[stderr]")
        || trimmed.contains("[orchestrator]")
        || trimmed.contains("[rules gate]")
        || trimmed.contains("c:\\")
        || trimmed.contains(".rs")
        || trimmed.contains(".svelte")
        || trimmed.contains(".ts")
        || trimmed.contains(".js")
        || trimmed.contains(".json")
        || trimmed.contains(".toml")
        || trimmed.contains(".md")
        || trimmed.contains("npm ")
        || trimmed.contains("cargo ")
        || trimmed.contains("git ");
    if has_code_or_path_signal {
        return false;
    }

    let work_words = [
        "agent",
        "analyze",
        "app",
        "auth",
        "backend",
        "build",
        "check",
        "cli",
        "code",
        "component",
        "create",
        "debug",
        "delegate",
        "deploy",
        "edit",
        "error",
        "file",
        "fix",
        "folder",
        "frontend",
        "implement",
        "inspect",
        "model",
        "orchestrator",
        "project",
        "read",
        "refactor",
        "research",
        "review",
        "route",
        "run",
        "search",
        "subagent",
        "tauri",
        "test",
        "update",
        "validate",
        "verify",
        "write",
    ];

    !work_words
        .iter()
        .any(|word| contains_word_like(trimmed, word))
}

fn contains_word_like(text: &str, needle: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .any(|part| part == needle)
}

fn is_known_model_for_provider(provider: &str, model: &str) -> bool {
    match provider {
        "claude" => matches!(
            model,
            "claude-opus-4.7" | "claude-sonnet-4.6" | "claude-haiku-4.5"
        ),
        "gemini" => matches!(
            model,
            "auto-gemini-3"
                | "auto-gemini-2.5"
                | "gemini-3.1-pro-preview"
                | "gemini-3-flash-preview"
                | "gemini-3.1-flash-lite-preview"
                | "gemini-2.5-pro"
                | "gemini-2.5-flash"
                | "gemini-2.5-flash-lite"
                | "gemma-4-31b-it"
                | "gemma-4-26b-a4b-it"
        ),
        "grok" => matches!(model, "grok-build"),
        "codex" => matches!(model, "gpt-5.5" | "gpt-5.3-codex" | "o3-pro" | "o3"),
        _ => false,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSubagentEvent {
    pub agent_key: String,
    pub name: String,
    pub role: String,
    pub model: String,
    pub task: Option<String>,
    pub parent_session_id: String,
    pub parent_run_id: String,
}

fn parse_spawn_command(
    line: &str,
    parent_session_id: &str,
    parent_run_id: &str,
) -> Option<SpawnSubagentEvent> {
    let content = line.strip_prefix("SPAWN_SUBAGENT:")?.trim();
    let mut agent_key = String::new();
    let mut task_value = String::new();

    for pair in content.split(',') {
        let trimmed_pair = pair.trim();
        let kv: Vec<&str> = pair.splitn(2, '=').collect();
        if kv.len() != 2 {
            if !task_value.is_empty() && !trimmed_pair.is_empty() {
                task_value.push_str(", ");
                task_value.push_str(trimmed_pair);
                continue;
            }
            return None;
        }

        let key = kv[0].trim().to_lowercase();
        let val = kv[1]
            .trim()
            .trim_matches(|c| c == '.' || c == ',' || c == ';' || c == '"' || c == '\'' || c == '`')
            .to_string();
        match key.as_str() {
            "agent" | "agent_key" => agent_key = val.to_lowercase(),
            "task" => task_value = val,
            _ => return None,
        }
    }

    if !is_canonical_agent_key(&agent_key) {
        return None;
    }

    Some(SpawnSubagentEvent {
        name: canonical_agent_name(&agent_key).to_string(),
        role: canonical_agent_role(&agent_key).to_string(),
        model: canonical_agent_default_provider(&agent_key).to_string(),
        agent_key,
        task: bounded_spawn_task(&task_value),
        parent_session_id: parent_session_id.to_string(),
        parent_run_id: parent_run_id.to_string(),
    })
}

fn bounded_spawn_task(task: &str) -> Option<String> {
    let trimmed = task.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.chars().take(MAX_SPAWN_TASK_CHARS).collect())
    }
}

fn is_canonical_agent_key(agent_key: &str) -> bool {
    matches!(
        agent_key,
        "research" | "backend" | "frontend" | "verification"
    )
}

fn canonical_agent_name(agent_key: &str) -> &'static str {
    match agent_key {
        "research" => "Research Agent",
        "backend" => "Backend Coder Agent",
        "frontend" => "Frontend Coder Agent",
        "verification" => "Verification Agent",
        _ => "Unknown Agent",
    }
}

fn canonical_agent_role(agent_key: &str) -> &'static str {
    match agent_key {
        "research" => "Codebase search & symbols",
        "backend" => "Rust / API / backend services",
        "frontend" => "Svelte / TS / styling design",
        "verification" => "Cargo check / test execution",
        _ => "Unknown role",
    }
}

fn canonical_agent_default_provider(agent_key: &str) -> &'static str {
    match agent_key {
        "research" => "grok",
        "backend" => "codex",
        "frontend" => "claude",
        "verification" => "gemini",
        _ => "claude",
    }
}

fn format_agent_registry(
    mappings: Option<&HashMap<String, String>>,
    provider_models: Option<&HashMap<String, String>>,
    subagent_models: Option<&HashMap<String, AgentModelSelection>>,
) -> String {
    let default_roles = [
        ("research", "Research Agent", "grok"),
        ("backend", "Backend Coder Agent", "codex"),
        ("frontend", "Frontend Coder Agent", "claude"),
        ("verification", "Verification Agent", "gemini"),
    ];

    default_roles
        .iter()
        .map(|(key, label, fallback_model)| {
            let registry_selection = subagent_models.and_then(|m| m.get(*key));
            let provider = registry_selection
                .map(|s| s.provider.as_str())
                .or_else(|| mappings.and_then(|m| m.get(*key)).map(|s| s.as_str()))
                .unwrap_or(*fallback_model);
            let specific = registry_selection
                .map(|s| s.specific_model.clone())
                .or_else(|| provider_models.and_then(|m| m.get(provider)).cloned())
                .unwrap_or_else(|| "provider-default".to_string());
            let active_flag = registry_selection
                .map(|s| {
                    if s.active {
                        "active=true"
                    } else {
                        "active=false"
                    }
                })
                .unwrap_or("active=unknown");
            let role_text = registry_selection.map(|s| s.role.as_str()).unwrap_or(label);
            format!(
                "- agent={}: label={} provider={} specific_model={} {} role={}",
                key, label, provider, specific, active_flag, role_text
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn select_specific_model(
    req: &PromptRequest,
    final_model: &str,
    routed_agent_key: Option<&str>,
) -> Option<String> {
    if let Some(agent_key) = routed_agent_key {
        if let Some(agent_specific) = req
            .subagent_models
            .as_ref()
            .and_then(|m| m.get(agent_key))
            .filter(|selection| selection.provider == final_model)
            .filter(|selection| {
                !selection
                    .specific_model
                    .eq_ignore_ascii_case("provider-default")
            })
            .map(|selection| selection.specific_model.clone())
        {
            return Some(agent_specific);
        }
    }

    if final_model == req.model {
        if let Some(specific) = req.specific_model.clone() {
            return Some(specific);
        }
    }

    req.provider_models
        .as_ref()
        .and_then(|m| m.get(final_model))
        .cloned()
}

pub fn route_agent_key(prompt: &str) -> Option<String> {
    let trimmed = prompt.trim();
    if trimmed.starts_with('@') {
        return None;
    }

    let lower_prompt = trimmed.to_lowercase();
    if lower_prompt.contains("research agent") || lower_prompt.contains("researcher") {
        return Some("research".to_string());
    }
    if lower_prompt.contains("backend coder agent") || lower_prompt.contains("backend coder") {
        return Some("backend".to_string());
    }
    if lower_prompt.contains("frontend coder agent") || lower_prompt.contains("frontend coder") {
        return Some("frontend".to_string());
    }
    if lower_prompt.contains("verification agent") || lower_prompt.contains("verifier") {
        return Some("verification".to_string());
    }
    None
}

pub fn route_task(
    prompt: &str,
    requested_model: &str,
    mappings: Option<&HashMap<String, String>>,
) -> String {
    let trimmed = prompt.trim();
    let lower_prompt = trimmed.to_lowercase();

    // Helper to get mapped model or fallback
    let get_mapped_model = |agent_key: &str, fallback: &str| -> String {
        if let Some(m) = mappings {
            if let Some(chosen) = m.get(agent_key) {
                return chosen.clone();
            }
        }
        fallback.to_string()
    };

    // 1. Explicit routing prefix like @grok, @claude, @codex, @gemini
    if trimmed.starts_with('@') {
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());
        if let Some(first) = parts.next() {
            let model_name = first.trim_start_matches('@').to_lowercase();
            if ["claude", "gemini", "grok", "codex"].contains(&model_name.as_str()) {
                return model_name;
            }
        }
    }

    // 2. Mentions of specific agents
    if lower_prompt.contains("research agent") || lower_prompt.contains("researcher") {
        return get_mapped_model("research", "grok");
    }
    if lower_prompt.contains("backend coder agent") || lower_prompt.contains("backend coder") {
        return get_mapped_model("backend", "codex");
    }
    if lower_prompt.contains("frontend coder agent") || lower_prompt.contains("frontend coder") {
        return get_mapped_model("frontend", "claude");
    }
    if lower_prompt.contains("verification agent") || lower_prompt.contains("verifier") {
        return get_mapped_model("verification", "gemini");
    }

    // 3. Fallback to requested model
    let req_model = requested_model.to_lowercase();
    if ["claude", "gemini", "grok", "codex"].contains(&req_model.as_str()) {
        req_model
    } else {
        "claude".to_string()
    }
}

pub fn clean_prompt(prompt: &str) -> String {
    let trimmed = prompt.trim();
    if trimmed.starts_with('@') {
        let mut parts = trimmed.splitn(2, |c: char| c.is_whitespace());
        if let Some(first) = parts.next() {
            let model_name = first.trim_start_matches('@').to_lowercase();
            if ["claude", "gemini", "grok", "codex"].contains(&model_name.as_str()) {
                return parts.next().unwrap_or("").trim().to_string();
            }
        }
    }
    trimmed.to_string()
}
