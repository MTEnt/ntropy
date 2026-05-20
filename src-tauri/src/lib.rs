mod cli_mediator;
mod rules_engine;
mod symbol_indexer;
mod ntropy_loop;

use cli_mediator::{CliMediator, PromptRequest};
use rules_engine::{RulesEngine, RulesManifest, Rule};
use symbol_indexer::{SymbolIndexer, FileSymbols};
use ntropy_loop::{NTropyLoopManager, SkillMetadata};


use std::path::{PathBuf, Path};
use std::fs;
use tauri::{AppHandle, State, Manager};

pub struct AppState {
    pub cli_mediator: CliMediator,
    pub rules_engine: RulesEngine,
    pub symbol_indexer: SymbolIndexer,
    pub ntropy_manager: NTropyLoopManager,
}

#[tauri::command]
fn run_cli_prompt(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    model: String,
    prompt: String,
    agent_mappings: Option<std::collections::HashMap<String, String>>,
) -> Result<(), String> {
    let workspace_root = state.symbol_indexer.workspace_root.lock().unwrap().clone();
    state.cli_mediator.send_prompt(app, PromptRequest {
        session_id,
        model,
        prompt,
        agent_mappings,
    }, workspace_root)
}

#[tauri::command]
fn get_rules(state: State<'_, AppState>) -> Result<RulesManifest, String> {
    state.rules_engine.get_rules()
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

#[tauri::command]
fn get_skills_index(state: State<'_, AppState>) -> Result<Vec<SkillMetadata>, String> {
    Ok(state.ntropy_manager.get_level0_index())
}

#[tauri::command]
fn get_skill_text(state: State<'_, AppState>, name: String) -> Result<String, String> {
    state.ntropy_manager.get_level1_detail(&name)
        .ok_or_else(|| format!("Skill '{}' not found", name))
}

#[tauri::command]
fn save_skill(
    state: State<'_, AppState>,
    name: String,
    description: String,
    triggers: Vec<String>,
    markdown: String,
) -> Result<String, String> {
    state.ntropy_manager.distill_new_skill(&name, &description, triggers, &markdown)
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
    let skills_dir = p.join(".agents").join("skills");
    let readme_file = p.join("README.md");
    
    if !src_dir.exists() {
        fs::create_dir_all(&src_dir)
            .map_err(|e| format!("Failed to create project src directory: {}", e))?;
    }
    if !skills_dir.exists() {
        fs::create_dir_all(&skills_dir)
            .map_err(|e| format!("Failed to create project skills directory: {}", e))?;
    }
    if !readme_file.exists() {
        let folder_name = p.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Agent OS Workspace".to_string());
        let readme_content = format!("# {}\n\nInitialized as an Agent OS workspace.", folder_name);
        fs::write(&readme_file, readme_content)
            .map_err(|e| format!("Failed to scaffold project README.md: {}", e))?;
    }
    
    state.symbol_indexer.set_workspace_root(p.clone());
    state.ntropy_manager.set_skills_dir(&p)?;
    
    list_project_files(state)
}

#[tauri::command]
fn create_project(state: State<'_, AppState>, parent_dir: String, project_name: String) -> Result<String, String> {
    let parent = PathBuf::from(&parent_dir);
    if !parent.exists() || !parent.is_dir() {
        return Err(format!("Parent directory does not exist: {}", parent_dir));
    }
    
    let project_path = parent.join(&project_name);
    if project_path.exists() {
        return Err(format!("Project directory already exists at: {:?}", project_path));
    }
    
    fs::create_dir_all(&project_path)
        .map_err(|e| format!("Failed to create project directory: {}", e))?;
    
    fs::create_dir_all(project_path.join("src"))
        .map_err(|e| format!("Failed to create src directory: {}", e))?;
    
    fs::create_dir_all(project_path.join(".agents").join("skills"))
        .map_err(|e| format!("Failed to create skills directory: {}", e))?;
    
    let readme_content = format!("# {}\n\nInitialized as an Agent OS workspace.", project_name);
    fs::write(project_path.join("README.md"), readme_content)
        .map_err(|e| format!("Failed to write README.md: {}", e))?;
        
    state.symbol_indexer.set_workspace_root(project_path.clone());
    state.ntropy_manager.set_skills_dir(&project_path)?;
    
    Ok(project_path.to_string_lossy().to_string())
}

#[tauri::command]
fn select_directory() -> Result<String, String> {
    let dir = rfd::FileDialog::new()
        .pick_folder();
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
            let relative = path.strip_prefix(root)
                .map_err(|e| format!("Strip prefix failed: {}", e))?
                .to_string_lossy()
                .replace("\\", "/");
            
            if relative.starts_with("node_modules") 
                || relative.starts_with("target") 
                || relative.starts_with(".git") 
                || relative.starts_with(".svelte-kit")
                || relative.starts_with(".vscode")
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

            // Initialize all core engines
            let state = AppState {
                cli_mediator: CliMediator::new(),
                rules_engine: RulesEngine::new(),
                symbol_indexer: SymbolIndexer::new(workspace_root.clone()),
                ntropy_manager: NTropyLoopManager::new(workspace_root),
            };

            app.manage(state);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            run_cli_prompt,
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
            select_directory
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
