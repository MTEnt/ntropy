use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    pub trigger_phrases: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Skill {
    pub metadata: SkillMetadata,
    pub file_name: String,
    pub content: String,
}

pub struct NTropyLoopManager {
    pub skills_dir: std::sync::Mutex<PathBuf>,
}

impl NTropyLoopManager {
    pub fn new<P: AsRef<Path>>(workspace_root: P) -> Self {
        let skills_dir = workspace_root.as_ref().join(".agents").join("skills");
        
        // Ensure directory exists
        if !skills_dir.exists() {
            if let Err(e) = fs::create_dir_all(&skills_dir) {
                eprintln!("[ERROR] NTropyLoopManager failed to create skills directory at {:?}: {}", skills_dir, e);
            }
        }

        Self { skills_dir: std::sync::Mutex::new(skills_dir) }
    }

    pub fn set_skills_dir(&self, workspace_root: &Path) -> Result<(), String> {
        let skills_dir = workspace_root.join(".agents").join("skills");
        // Ensure directory exists
        if !skills_dir.exists() {
            fs::create_dir_all(&skills_dir)
                .map_err(|e| format!("Failed to create skills directory at {:?}: {}", skills_dir, e))?;
        }
        let mut dir = self.skills_dir.lock().unwrap();
        *dir = skills_dir;
        Ok(())
    }

    /// Level 0: Progressive Disclosure
    /// Returns a list of all skills names and descriptions (low token cost).
    pub fn get_level0_index(&self) -> Vec<SkillMetadata> {
        let mut list = Vec::new();
        let dir = self.skills_dir.lock().unwrap().clone();
        if let Ok(entries) = fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        if let Some(meta) = self.parse_skill_metadata(&content) {
                            list.push(meta);
                        }
                    }
                }
            }
        }
        list
    }

    /// Level 1: Progressive Disclosure
    /// Returns the full content of a skill.
    pub fn get_level1_detail(&self, skill_name: &str) -> Option<String> {
        let file_name = format!("{}.md", skill_name.to_lowercase().replace(" ", "_"));
        let path = Path::new(&file_name);
        if path.is_absolute() || path.components().any(|c| c == std::path::Component::ParentDir) {
            return None;
        }
        let dir = self.skills_dir.lock().unwrap().clone();
        let skill_path = dir.join(file_name);
        fs::read_to_string(skill_path).ok()
    }

    /// Autonomous Skill Distiller:
    /// Packages a new skill and writes it to the skills folder as an on-demand executable skill standard.
    pub fn distill_new_skill(
        &self,
        name: &str,
        description: &str,
        triggers: Vec<String>,
        workflow_markdown: &str,
    ) -> Result<String, String> {
        let file_name = format!("{}.md", name.to_lowercase().replace(" ", "_"));
        let path = Path::new(&file_name);
        if path.is_absolute() || path.components().any(|c| c == std::path::Component::ParentDir) {
            return Err("Security Violation: Path traversal or absolute path detected".to_string());
        }
        let dir = self.skills_dir.lock().unwrap().clone();
        let skill_path = dir.join(&file_name);

        // Construct standard SKILL.md format
        let mut content = String::new();
        content.push_str(&format!("# Skill: {}\n\n", name));
        content.push_str(&format!("> Description: {}\n", description));
        content.push_str(&format!("> Triggers: {}\n\n", triggers.join(", ")));
        content.push_str("## Core Action Steps\n\n");
        content.push_str(workflow_markdown);
        content.push_str("\n\n## Verification Verification Standard\n");
        content.push_str("- Test run command verifies accuracy.\n");

        fs::write(&skill_path, &content)
            .map_err(|e| format!("Failed to save distilled skill: {}", e))?;

        println!("Autonomous learning loop: distilled new skill '{}' to {:?}", name, skill_path);
        Ok(file_name)
    }

    fn parse_skill_metadata(&self, md_content: &str) -> Option<SkillMetadata> {
        let mut lines = md_content.lines();
        let name_line = lines.next()?;
        if !name_line.starts_with("# Skill:") {
            return None;
        }
        let name = name_line.trim_start_matches("# Skill:").trim().to_string();

        let desc_line = lines.next()?;
        let description = desc_line.trim_start_matches("> Description:").trim().to_string();

        let trigger_line = lines.next()?;
        let triggers = trigger_line
            .trim_start_matches("> Triggers:")
            .split(',')
            .map(|s| s.trim().to_string())
            .collect::<Vec<String>>();

        Some(SkillMetadata {
            name,
            description,
            trigger_phrases: triggers,
        })
    }
}
