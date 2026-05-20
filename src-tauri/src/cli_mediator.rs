use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::process::{Command, Child, Stdio};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptRequest {
    pub session_id: String,
    pub model: String, // "claude", "gemini", "grok", "codex"
    pub prompt: String,
    pub agent_mappings: Option<HashMap<String, String>>,
    pub provider_models: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutputEvent {
    pub session_id: String,
    pub stream: String, // "stdout" or "stderr"
    pub data: String,
}

pub struct CliSession {
    pub child: Child,
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

    pub fn send_prompt(&self, app: AppHandle, req: PromptRequest, workspace_root: std::path::PathBuf) -> Result<(), String> {
        let mut sessions = self.active_sessions.lock().unwrap();
        let session_id = req.session_id.clone();
        
        let final_model = route_task(&req.prompt, &req.model, req.agent_mappings.as_ref());
        let specific_model = req.provider_models.as_ref()
            .and_then(|m| m.get(&final_model))
            .cloned();
            
        let cleaned_prompt = clean_prompt(&req.prompt);
        let prompt_with_harness = format!(
            "{}\n\n[SYSTEM INSTRUCTION]: You are running in the nTropy workspace environment. You have the capability to dynamically spawn specialized subagents to run concurrent tasks for you. If you choose to spawn subagents, print one or more lines in this exact format (each on its own line):\nSPAWN_SUBAGENT:name=<Name>,role=<Role>,model=<claude|gemini|grok|codex>\nYou can spawn as many subagents as you deem necessary to complete the task.",
            cleaned_prompt
        );
        
        // Terminate any existing running process for this session first
        if let Some(mut old_session) = sessions.remove(&session_id) {
            println!("Killing previous running process for session '{}'", session_id);
            if let Err(e) = old_session.child.kill() {
                println!("Warning: Failed to terminate previous running process for session '{}' (process may have already terminated): {}", session_id, e);
            }
        }

        // Print visual confirmation of orchestrator routing and folder harness to user stdout stream
        let display_model_info = if let Some(ref m) = specific_model {
            format!("{} ({})", final_model.to_uppercase(), m)
        } else {
            final_model.to_uppercase()
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

        println!("Spawning new CLI process for model '{}' in session '{}' inside directory {:?}", final_model, session_id, workspace_root);

        let mut child = if cfg!(target_os = "windows") {
            let mut spawned = None;
            
            // Try spawning direct native executable first to bypass cmd.exe and avoid escaping bugs
            match final_model.as_str() {
                "claude" => {
                    let direct_path = r"C:\Users\User\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe";
                    let mut cmd = Command::new(direct_path);
                    cmd.current_dir(&workspace_root)
                       .arg("--print")
                       .arg("--dangerously-skip-permissions")
                       .stdin(Stdio::piped())
                       .stdout(Stdio::piped())
                       .stderr(Stdio::piped());
                    if let Some(ref m) = specific_model {
                        cmd.arg("--model").arg(m);
                        cmd.env("CLAUDE_MODEL", m);
                        cmd.env("LLM_MODEL", m);
                    }
                    if let Ok(c) = cmd.spawn() {
                        println!("Direct claude.exe spawned successfully.");
                        spawned = Some(c);
                    }
                }
                "grok" => {
                    let direct_path = r"C:\Users\User\.grok\bin\grok.exe";
                    let mut cmd = Command::new(direct_path);
                    cmd.current_dir(&workspace_root)
                       .stdin(Stdio::piped())
                       .stdout(Stdio::piped())
                       .stderr(Stdio::piped());
                    
                    let mut args = vec!["-c"];
                    if let Some(ref m) = specific_model {
                        args.push("-m");
                        args.push(m);
                        cmd.env("GROK_MODEL", m);
                        cmd.env("LLM_MODEL", m);
                    }
                    args.push("-p");
                    args.push(&prompt_with_harness);
                    cmd.args(&args);

                    if let Ok(c) = cmd.spawn() {
                        println!("Direct grok.exe spawned successfully.");
                        spawned = Some(c);
                    }
                }
                "codex" => {
                    let direct_path = r"C:\Users\User\AppData\Roaming\npm\node_modules\@openai\codex\node_modules\@openai\codex-win32-x64\vendor\x86_64-pc-windows-msvc\codex\codex.exe";
                    let mut cmd = Command::new(direct_path);
                    cmd.current_dir(&workspace_root)
                       .stdin(Stdio::piped())
                       .stdout(Stdio::piped())
                       .stderr(Stdio::piped());
                       
                    if let Some(ref m) = specific_model {
                        cmd.env("OPENAI_MODEL", m);
                        cmd.env("LLM_MODEL", m);
                    }
                    
                    // Try to resume the last session
                    let mut args = vec!["exec"];
                    if let Some(ref m) = specific_model {
                        args.push("--model");
                        args.push(m);
                    }
                    
                    let mut resume_args = args.clone();
                    resume_args.push("resume");
                    resume_args.push("--last");
                    resume_args.push("--skip-git-repo-check");
                    resume_args.push("-");
                    
                    let mut fallback_args = args.clone();
                    fallback_args.push("--skip-git-repo-check");
                    fallback_args.push("-");
                    
                    if let Ok(c) = cmd.args(&resume_args).spawn() {
                        println!("Direct codex.exe (resume last) spawned successfully.");
                        spawned = Some(c);
                    } else {
                        let mut cmd2 = Command::new(direct_path);
                        cmd2.current_dir(&workspace_root)
                           .stdin(Stdio::piped())
                           .stdout(Stdio::piped())
                           .stderr(Stdio::piped());
                        if let Some(ref m) = specific_model {
                            cmd2.env("OPENAI_MODEL", m);
                            cmd2.env("LLM_MODEL", m);
                        }
                        if let Ok(c) = cmd2.args(&fallback_args).spawn() {
                            println!("Direct codex.exe spawned successfully.");
                            spawned = Some(c);
                        }
                    }
                }
                _ => {}
            }

            // Fallback to cmd /C if direct native spawn failed or for other models
            if spawned.is_none() {
                println!("Falling back to cmd /C for model: {}", final_model);
                let mut cmd = Command::new("cmd");
                cmd.current_dir(&workspace_root);
                cmd.arg("/C");
                
                if let Some(ref m) = specific_model {
                    cmd.env("CLAUDE_MODEL", m);
                    cmd.env("GEMINI_MODEL", m);
                    cmd.env("GROK_MODEL", m);
                    cmd.env("OPENAI_MODEL", m);
                    cmd.env("LLM_MODEL", m);
                }
                
                match final_model.as_str() {
                    "claude" => {
                        let mut args = vec!["claude", "--print", "--dangerously-skip-permissions"];
                        if let Some(ref m) = specific_model {
                            args.push("--model");
                            args.push(m);
                        }
                        cmd.args(&args);
                    }
                    "grok" => {
                        let mut args = vec!["grok", "-c"];
                        if let Some(ref m) = specific_model {
                            args.push("-m");
                            args.push(m);
                        }
                        args.push("-p");
                        args.push(&prompt_with_harness);
                        cmd.args(&args);
                    }
                    "codex" => {
                        let mut args = vec!["codex", "exec", "resume", "--last", "--skip-git-repo-check"];
                        if let Some(ref m) = specific_model {
                            args.push("--model");
                            args.push(m);
                        }
                        args.push(&prompt_with_harness);
                        cmd.args(&args);
                    }
                    "gemini" => {
                        let mut args = vec!["gemini"];
                        if let Some(ref m) = specific_model {
                            args.push("--model");
                            args.push(m);
                        }
                        cmd.args(&args);
                    }
                    _ => {
                        return Err(format!("Unsupported model/CLI: {}", final_model));
                    }
                }
                cmd.stdin(Stdio::piped())
                   .stdout(Stdio::piped())
                   .stderr(Stdio::piped())
                   .spawn()
                   .map_err(|e| format!("Failed to spawn cmd fallback: {}", e))?
            } else {
                spawned.unwrap()
            }
        } else {
            // Non-windows fallback
            let mut cmd = Command::new(match final_model.as_str() {
                "claude" => "claude",
                "gemini" => "gemini",
                "grok" => "grok",
                "codex" => "codex",
                _ => return Err(format!("Unsupported model/CLI: {}", final_model)),
            });
            cmd.current_dir(&workspace_root);
            
            if let Some(ref m) = specific_model {
                cmd.env("CLAUDE_MODEL", m);
                cmd.env("GEMINI_MODEL", m);
                cmd.env("GROK_MODEL", m);
                cmd.env("OPENAI_MODEL", m);
                cmd.env("LLM_MODEL", m);
            }

            match final_model.as_str() {
                "claude" => {
                    let mut args = vec!["--print", "--dangerously-skip-permissions"];
                    if let Some(ref m) = specific_model {
                        args.push("--model");
                        args.push(m);
                    }
                    cmd.args(&args);
                }
                "grok" => {
                    let mut args = vec!["-c"];
                    if let Some(ref m) = specific_model {
                        args.push("-m");
                        args.push(m);
                    }
                    args.push("-p");
                    args.push(&prompt_with_harness);
                    cmd.args(&args);
                }
                "codex" => {
                    let mut args = vec!["exec", "resume", "--last", "--skip-git-repo-check"];
                    if let Some(ref m) = specific_model {
                        args.push("--model");
                        args.push(m);
                    }
                    args.push("-");
                    cmd.args(&args);
                }
                "gemini" => {
                    let mut args = Vec::new();
                    if let Some(ref m) = specific_model {
                        args.push("--model");
                        args.push(m);
                    }
                    cmd.args(&args);
                }
                _ => {}
            }
            cmd.stdin(Stdio::piped())
               .stdout(Stdio::piped())
               .stderr(Stdio::piped())
               .spawn()
               .map_err(|e| format!("Failed to spawn CLI: {}", e))?
        };

        // If stdin is piped, write the prompt to it and immediately close it
        if let Some(mut stdin) = child.stdin.take() {
            if final_model == "claude" || final_model == "codex" || final_model == "gemini" {
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

        // Spawn background threads to continuously read stdout and stderr chunk-by-chunk in real-time
        let app_stdout = app.clone();
        let session_id_stdout = session_id.clone();
        std::thread::spawn(move || {
            let mut reader = stdout;
            let mut buf = [0u8; 1024];
            let mut line_buffer = String::new();
            
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        let chunk_str = String::from_utf8_lossy(&buf[..n]).to_string();
                        
                        let _ = app_stdout.emit("cli-output", ProcessOutputEvent {
                            session_id: session_id_stdout.clone(),
                            stream: "stdout".to_string(),
                            data: chunk_str.clone(),
                        });

                        // Accumulate line_buffer for SPAWN_SUBAGENT detection
                        line_buffer.push_str(&chunk_str);
                        while let Some(pos) = line_buffer.find('\n') {
                            let line = line_buffer[..pos].trim_end().to_string();
                            line_buffer = line_buffer[pos + 1..].to_string();
                            
                            if line.contains("SPAWN_SUBAGENT:") {
                                if let Some(spawn_req) = parse_spawn_command(&line) {
                                    println!("Dynamic Subagent Spawn Request intercepted: {:?}", spawn_req);
                                    let _ = app_stdout.emit("spawn-subagent", spawn_req);
                                }
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Failed to read stdout stream chunk for session '{}': {}", session_id_stdout, e);
                        break;
                    }
                }
            }
            
            // Signal to frontend that the CLI run is complete
            let _ = app_stdout.emit("cli-finished", session_id_stdout);
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
                        let _ = app_stderr.emit("cli-output", ProcessOutputEvent {
                            session_id: session_id_stderr.clone(),
                            stream: "stderr".to_string(),
                            data: chunk_str,
                        });
                    }
                    Err(e) => {
                        eprintln!("[ERROR] Failed to read stderr stream chunk for session '{}': {}", session_id_stderr, e);
                        break;
                    }
                }
            }
        });

        sessions.insert(session_id, CliSession { child });

        Ok(())
    }

    pub fn terminate_session(&self, session_id: &str) {
        let mut sessions = self.active_sessions.lock().unwrap();
        if let Some(mut session) = sessions.remove(session_id) {
            println!("Terminating CLI session '{}'", session_id);
            let _ = session.child.kill();
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnSubagentEvent {
    pub name: String,
    pub role: String,
    pub model: String,
}

fn parse_spawn_command(line: &str) -> Option<SpawnSubagentEvent> {
    let parts: Vec<&str> = line.split("SPAWN_SUBAGENT:").collect();
    if parts.len() < 2 {
        return None;
    }
    
    let content = parts[1].trim();
    let mut name = String::new();
    let mut role = String::new();
    let mut model = String::from("claude");
    
    for pair in content.split(',') {
        let kv: Vec<&str> = pair.split('=').collect();
        if kv.len() == 2 {
            let key = kv[0].trim().to_lowercase();
            let mut val = kv[1].trim().to_string();
            // Trim common trailing punctuation or markdown enclosing characters
            val = val.trim_matches(|c| c == '.' || c == ',' || c == ';' || c == '"' || c == '\'' || c == '`').to_string();
            match key.as_str() {
                "name" => name = val,
                "role" => role = val,
                "model" => model = val.to_lowercase(),
                _ => {}
            }
        }
    }
    
    if !name.is_empty() && !role.is_empty() {
        Some(SpawnSubagentEvent { name, role, model })
    } else {
        None
    }
}

pub fn route_task(prompt: &str, requested_model: &str, mappings: Option<&HashMap<String, String>>) -> String {
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

    // 3. Keyword-based classification routing
    
    // Research / Exploration -> Grok CLI
    let research_words = [
        "research", "search", "find", "symbols", "explain", "codebase", 
        "grep", "lookup", "locate", "explain how", "investigate", "analyze", 
        "history", "what does", "how does"
    ];
    if research_words.iter().any(|&word| lower_prompt.contains(word)) {
        return get_mapped_model("research", "grok");
    }

    // Backend Coder -> Codex CLI
    let backend_words = [
        "backend", "rust", "api", "cargo", "database", "db", "server", 
        "endpoint", "sql", "handler", "route", "backend coder", "codex",
        "actix", "axum", "diesel", "tokio"
    ];
    if backend_words.iter().any(|&word| lower_prompt.contains(word)) {
        return get_mapped_model("backend", "codex");
    }

    // Frontend Coder -> Claude CLI
    let frontend_words = [
        "frontend", "svelte", "ts", "css", "html", "ui", "style", 
        "component", "button", "layout", "div", "page", "client", 
        "frontend coder", "claude", "navbar", "sidebar", "flexbox", 
        "grid", "tailwind", "responsive"
    ];
    if frontend_words.iter().any(|&word| lower_prompt.contains(word)) {
        return get_mapped_model("frontend", "claude");
    }

    // Verification -> Gemini CLI
    let verification_words = [
        "verify", "test", "check", "run tests", "compile", "audit", 
        "lint", "validate", "verification", "gemini", "assert", 
        "cargo check", "cargo test", "npm run check"
    ];
    if verification_words.iter().any(|&word| lower_prompt.contains(word)) {
        return get_mapped_model("verification", "gemini");
    }

    // 4. Fallback to requested model
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
