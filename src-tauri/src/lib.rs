mod cli_mediator;
mod db;
mod rules_engine;
mod symbol_indexer;

use cli_mediator::{AgentModelSelection, CliMediator, PromptRequest};
use rules_engine::{Rule, RulesEngine, RulesManifest};
use symbol_indexer::{FileSymbols, SymbolIndexer};

use std::fs;
use std::path::{Component, Path, PathBuf};
use tauri::{AppHandle, Manager, State};

#[derive(serde::Serialize, serde::Deserialize, Debug, Clone)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub trigger_phrases: Vec<String>,
}

pub struct AppState {
    pub cli_mediator: CliMediator,
    pub rules_engine: RulesEngine,
    pub symbol_indexer: SymbolIndexer,
    pub db: std::sync::Mutex<std::sync::Arc<db::Database>>,
}

#[tauri::command]
fn run_cli_prompt(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    run_id: Option<String>,
    model: String,
    specific_model: Option<String>,
    prompt: String,
    agent_mappings: Option<std::collections::HashMap<String, String>>,
    provider_models: Option<std::collections::HashMap<String, String>>,
    subagent_models: Option<std::collections::HashMap<String, AgentModelSelection>>,
) -> Result<(), String> {
    let workspace_root = state.symbol_indexer.workspace_root.lock().unwrap().clone();
    state.cli_mediator.send_prompt(
        app,
        PromptRequest {
            session_id,
            run_id,
            model,
            specific_model,
            prompt,
            agent_mappings,
            provider_models,
            subagent_models,
        },
        workspace_root,
    )
}

#[tauri::command]
fn terminate_session(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    state.cli_mediator.terminate_session(&session_id)
}

#[tauri::command]
fn get_rules(state: State<'_, AppState>) -> Result<RulesManifest, String> {
    state
        .rules_engine
        .get_rules()
        .ok_or_else(|| "Rules not initialized yet".to_string())
}

#[tauri::command]
fn check_rules_action(
    state: State<'_, AppState>,
    trigger: String,
    content: String,
) -> Result<Vec<Rule>, String> {
    Ok(state.rules_engine.check_action(&trigger, &content))
}

#[tauri::command]
fn index_symbols(state: State<'_, AppState>, relative_path: String) -> Result<FileSymbols, String> {
    state.symbol_indexer.index_file(&relative_path)
}

// --- Dynamic Swapped project Database Commands ---

#[tauri::command]
async fn get_all_sessions(
    state: State<'_, AppState>,
) -> Result<Vec<(String, String, String, String)>, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.get_all_sessions().await
}

#[tauri::command]
async fn create_session(
    state: State<'_, AppState>,
    id: String,
    title: String,
) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.create_session(id, title).await
}

#[tauri::command]
async fn delete_session(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.delete_session(id).await
}

#[tauri::command]
async fn get_session_messages(
    state: State<'_, AppState>,
    session_id: String,
) -> Result<Vec<(String, String, String)>, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.get_session_messages(session_id).await
}

#[tauri::command]
async fn add_session_message(
    state: State<'_, AppState>,
    session_id: String,
    role: String,
    content: String,
) -> Result<i64, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.add_session_message(session_id, role, content).await
}

#[tauri::command]
async fn update_session_message(
    state: State<'_, AppState>,
    id: i64,
    content: String,
) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.update_session_message(id, content).await
}

#[tauri::command]
async fn get_all_tasks(
    state: State<'_, AppState>,
) -> Result<Vec<(String, String, String, String, String, String)>, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.get_all_tasks().await
}

#[tauri::command]
async fn create_task(
    state: State<'_, AppState>,
    id: String,
    session_id: String,
    text: String,
) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.create_task(id, session_id, text).await
}

#[tauri::command]
async fn update_task_status(
    state: State<'_, AppState>,
    id: String,
    status: String,
) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.update_task_status(id, status).await
}

#[tauri::command]
async fn delete_task(state: State<'_, AppState>, id: String) -> Result<(), String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    db.delete_task(id).await
}

// --- HTML-Based Skills database queries ---

#[tauri::command]
async fn get_skills_index(state: State<'_, AppState>) -> Result<Vec<SkillMetadata>, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    let list = db.get_all_skills().await?;
    let mut mapped = Vec::new();
    for (name, description, trigger_tags, _, enabled) in list {
        if enabled {
            let triggers = trigger_tags
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>();
            mapped.push(SkillMetadata {
                name,
                description,
                trigger_phrases: triggers,
            });
        }
    }

    // Seed default skills if empty to guarantee beautiful user onboarding
    if mapped.is_empty() {
        let default_skills = vec![
            (
                "Safe Git Push",
                "Validates, commits, and pushes code under MTEnt identity.",
                "push updates,git push",
                r#"<skill name="Safe Git Push" description="Validates, commits, and pushes code under MTEnt identity.">
  <triggers>
    <trigger>push updates</trigger>
    <trigger>git push</trigger>
  </triggers>
  <steps>
    <step>Verify git status has no stray or untracked changes.</step>
    <step>Commit modified files with a clear description on behalf of MTEnt.</step>
    <step>Push the commits safely to origin main.</step>
  </steps>
</skill>"#,
            ),
            (
                "HTML Prompt Refactor",
                "Refactors procedural flows to use precise structural HTML tags.",
                "refactor prompt,html skill",
                r#"<skill name="HTML Prompt Refactor" description="Refactors procedural flows to use precise structural HTML tags.">
  <triggers>
    <trigger>refactor prompt</trigger>
    <trigger>html skill</trigger>
  </triggers>
  <steps>
    <step>Extract the loose procedural instructions from prompts.</step>
    <step>Translate instructions into semantic HTML elements like &lt;steps&gt; and &lt;step&gt;.</step>
    <step>Inject the clean, boundary-accurate HTML block back into the orchestrator context.</step>
  </steps>
</skill>"#,
            ),
        ];

        for (name, desc, triggers, body) in default_skills {
            let _ = db
                .create_or_update_skill(
                    name.to_string(),
                    desc.to_string(),
                    triggers.to_string(),
                    body.to_string(),
                )
                .await;
        }

        let list2 = db.get_all_skills().await?;
        for (name, description, trigger_tags, _, enabled) in list2 {
            if enabled {
                let triggers = trigger_tags
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>();
                mapped.push(SkillMetadata {
                    name,
                    description,
                    trigger_phrases: triggers,
                });
            }
        }
    }

    Ok(mapped)
}

#[tauri::command]
async fn get_skill_text(state: State<'_, AppState>, name: String) -> Result<String, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    let list = db.get_all_skills().await?;
    for (skill_name, _, _, definition, _) in list {
        if skill_name.to_lowercase() == name.to_lowercase() {
            return Ok(definition);
        }
    }
    Err(format!("Skill '{}' not found in database", name))
}

#[tauri::command]
async fn save_skill(
    state: State<'_, AppState>,
    name: String,
    description: String,
    triggers: Vec<String>,
    markdown: String, // Kept parameter name 'markdown' to not break Svelte bindings
) -> Result<String, String> {
    let db = {
        let guard = state.db.lock().unwrap();
        guard.clone()
    };
    let trigger_tags = triggers.join(",");
    db.create_or_update_skill(name.clone(), description, trigger_tags, markdown)
        .await?;
    Ok(name)
}

#[tauri::command]
fn get_active_project(state: State<'_, AppState>) -> Result<String, String> {
    let root = state.symbol_indexer.workspace_root.lock().unwrap().clone();
    Ok(root.to_string_lossy().to_string())
}

#[tauri::command]
fn open_project(state: State<'_, AppState>, path: String) -> Result<Vec<String>, String> {
    let p = PathBuf::from(&path);
    if !p.exists() || !p.is_dir() {
        return Err(format!("Directory does not exist: {}", path));
    }

    // Automatically scaffold missing project workspace structures
    let src_dir = p.join("src");
    let agents_dir = p.join(".agents");
    let readme_file = p.join("README.md");

    if !src_dir.exists() {
        fs::create_dir_all(&src_dir)
            .map_err(|e| format!("Failed to create project src directory: {}", e))?;
    }
    if !agents_dir.exists() {
        fs::create_dir_all(&agents_dir)
            .map_err(|e| format!("Failed to create project agents directory: {}", e))?;
    }
    if !readme_file.exists() {
        let folder_name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Agent OS Workspace".to_string());
        let readme_content = format!("# {}\n\nInitialized as an Agent OS workspace.", folder_name);
        fs::write(&readme_file, readme_content)
            .map_err(|e| format!("Failed to scaffold project README.md: {}", e))?;
    }

    // Dynamically swap project SQLite Connection
    let db_path = agents_dir.join("nentropy.db");
    let db_str = db_path.to_string_lossy().to_string();
    let new_db = tauri::async_runtime::block_on(async { db::Database::open(&db_str).await })?;

    {
        let mut db_guard = state.db.lock().unwrap();
        *db_guard = std::sync::Arc::new(new_db);
    }

    state.symbol_indexer.set_workspace_root(p.clone());

    list_project_files(state)
}

#[tauri::command]
fn create_project(
    state: State<'_, AppState>,
    parent_dir: String,
    project_name: String,
) -> Result<String, String> {
    let parent = fs::canonicalize(PathBuf::from(&parent_dir))
        .map_err(|e| format!("Parent directory is not accessible: {}", e))?;
    if !parent.exists() || !parent.is_dir() {
        return Err(format!("Parent directory does not exist: {}", parent_dir));
    }

    let project_name = validate_project_name(&project_name)?;

    let project_path = parent.join(&project_name);
    if !project_path.starts_with(&parent) {
        return Err("Project path must stay inside the selected parent directory".to_string());
    }
    if project_path.exists() {
        return Err(format!(
            "Project directory already exists at: {:?}",
            project_path
        ));
    }

    fs::create_dir_all(&project_path)
        .map_err(|e| format!("Failed to create project directory: {}", e))?;

    fs::create_dir_all(project_path.join("src"))
        .map_err(|e| format!("Failed to create src directory: {}", e))?;

    fs::create_dir_all(project_path.join(".agents"))
        .map_err(|e| format!("Failed to create agents directory: {}", e))?;

    let readme_content = format!(
        "# {}\n\nInitialized as an Agent OS workspace.",
        project_name
    );
    fs::write(project_path.join("README.md"), readme_content)
        .map_err(|e| format!("Failed to write README.md: {}", e))?;

    // Dynamically swap project SQLite Connection
    let db_path = project_path.join(".agents").join("nentropy.db");
    let db_str = db_path.to_string_lossy().to_string();
    let new_db = tauri::async_runtime::block_on(async { db::Database::open(&db_str).await })?;

    {
        let mut db_guard = state.db.lock().unwrap();
        *db_guard = std::sync::Arc::new(new_db);
    }

    state
        .symbol_indexer
        .set_workspace_root(project_path.clone());

    Ok(project_path.to_string_lossy().to_string())
}

fn validate_project_name(project_name: &str) -> Result<String, String> {
    let trimmed = project_name.trim();
    if trimmed.is_empty() {
        return Err("Project name cannot be empty".to_string());
    }
    if trimmed.len() > 120 {
        return Err("Project name is too long".to_string());
    }
    if trimmed.ends_with('.') || trimmed.ends_with(' ') {
        return Err("Project name cannot end with a dot or space".to_string());
    }
    if trimmed.chars().any(|c| {
        c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    }) {
        return Err("Project name contains unsupported path characters".to_string());
    }

    let mut components = Path::new(trimmed).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => return Err("Project name must be a single folder name".to_string()),
    }

    let base_name = trimmed
        .split('.')
        .next()
        .unwrap_or(trimmed)
        .to_ascii_uppercase();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if reserved.contains(&base_name.as_str()) {
        return Err("Project name is reserved by the operating system".to_string());
    }

    Ok(trimmed.to_string())
}

#[tauri::command]
fn select_directory() -> Result<String, String> {
    let dir = rfd::FileDialog::new().pick_folder();
    match dir {
        Some(path) => Ok(path.to_string_lossy().to_string()),
        None => Err("No directory selected".to_string()),
    }
}

#[tauri::command]
fn list_project_files(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let root = state.symbol_indexer.workspace_root.lock().unwrap().clone();
    let mut files = Vec::new();
    walk_dir_recursive(&root, &root, &mut files)?;
    Ok(files)
}

fn walk_dir_recursive(root: &Path, current: &Path, acc: &mut Vec<String>) -> Result<(), String> {
    if let Ok(entries) = fs::read_dir(current) {
        for entry in entries.flatten() {
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .map_err(|e| format!("Strip prefix failed: {}", e))?
                .to_string_lossy()
                .replace("\\", "/");

            if relative.starts_with("node_modules")
                || relative.starts_with("target")
                || relative.starts_with(".git")
                || relative.starts_with(".svelte-kit")
                || relative.starts_with(".vscode")
                || relative.starts_with(".agents")
            {
                continue;
            }

            if path.is_dir() {
                walk_dir_recursive(root, &path, acc)?;
            } else {
                acc.push(relative);
            }
        }
    }
    Ok(())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            // Root paths based on Desktop conventions
            let workspace_root = PathBuf::from(r"C:\Users\User\Desktop\auto-os");

            // Ensure .agents exists
            let agents_dir = workspace_root.join(".agents");
            if !agents_dir.exists() {
                let _ = fs::create_dir_all(&agents_dir);
            }

            // Open SQLite Database connection
            let db_path = agents_dir.join("nentropy.db");
            let db_str = db_path.to_string_lossy().to_string();
            let db = tauri::async_runtime::block_on(async {
                db::Database::open(&db_str)
                    .await
                    .expect("Failed to initialize SQLite database")
            });

            // Initialize all core engines
            let state = AppState {
                cli_mediator: CliMediator::new(),
                rules_engine: RulesEngine::new(),
                symbol_indexer: SymbolIndexer::new(workspace_root.clone()),
                db: std::sync::Mutex::new(std::sync::Arc::new(db)),
            };

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            run_cli_prompt,
            terminate_session,
            get_rules,
            check_rules_action,
            index_symbols,
            get_skills_index,
            get_skill_text,
            save_skill,
            get_active_project,
            open_project,
            create_project,
            list_project_files,
            select_directory,
            get_all_sessions,
            create_session,
            delete_session,
            get_session_messages,
            add_session_message,
            update_session_message,
            get_all_tasks,
            create_task,
            update_task_status,
            delete_task
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
