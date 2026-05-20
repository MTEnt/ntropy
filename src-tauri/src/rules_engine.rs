use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub severity: String,
    pub trigger: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesManifest {
    pub non_negotiables: Vec<Rule>,
    pub operating_loop: Vec<String>,
}

#[derive(Clone)]
pub struct RulesEngine {
    current_rules: Arc<Mutex<Option<RulesManifest>>>,
}

fn get_default_ruleset() -> RulesManifest {
    RulesManifest {
        non_negotiables: vec![
            Rule {
                id: "NN-INSPECT-FIRST".to_string(),
                severity: "blocking".to_string(),
                trigger: "before-edit".to_string(),
                text: "Inspect relevant code, tests, configs, and execution paths before editing.".to_string(),
            },
            Rule {
                id: "NN-NO-UNRELATED-CHANGES".to_string(),
                severity: "blocking".to_string(),
                trigger: "during-edit".to_string(),
                text: "Do not make unrelated changes, drive-by formatting, or opportunistic rewrites.".to_string(),
            },
            Rule {
                id: "NN-NO-ARCHITECTURE-REWRITE".to_string(),
                severity: "blocking".to_string(),
                trigger: "before-refactor".to_string(),
                text: "Do not rewrite architecture unless explicitly requested or strictly necessary to satisfy the task.".to_string(),
            },
            Rule {
                id: "NN-DEPENDENCIES-JUSTIFIED".to_string(),
                severity: "blocking".to_string(),
                trigger: "before-new-dependency".to_string(),
                text: "Do not add, remove, upgrade, or swap dependencies without evidence and justification.".to_string(),
            },
            Rule {
                id: "NN-TESTS-SACRED".to_string(),
                severity: "blocking".to_string(),
                trigger: "when-tests-fail".to_string(),
                text: "Do not delete, weaken, or bypass tests to make a suite pass.".to_string(),
            },
            Rule {
                id: "NN-HONEST-VERIFICATION".to_string(),
                severity: "blocking".to_string(),
                trigger: "before-final-response".to_string(),
                text: "Do not claim verification that was not performed; disclose commands run and commands not run.".to_string(),
            },
            Rule {
                id: "NN-SECURITY-CORRECTNESS".to_string(),
                severity: "blocking".to_string(),
                trigger: "always".to_string(),
                text: "Treat security and privacy as correctness requirements.".to_string(),
            },
            Rule {
                id: "NN-REPORT-UNCERTAINTY".to_string(),
                severity: "high".to_string(),
                trigger: "before-final-response".to_string(),
                text: "Report assumptions, uncertainty, residual risk, and unverified areas explicitly.".to_string(),
            },
        ],
        operating_loop: vec![
            "orient".to_string(),
            "investigate".to_string(),
            "decide".to_string(),
            "execute".to_string(),
            "verify".to_string(),
            "report".to_string(),
        ],
    }
}

impl RulesEngine {
    pub fn new() -> Self {
        let manifest = get_default_ruleset();
        println!("Successfully initialized baked-in Human Code Rules v5.1 within the application.");
        Self {
            current_rules: Arc::new(Mutex::new(Some(manifest))),
        }
    }

    pub fn get_rules(&self) -> Option<RulesManifest> {
        self.current_rules.lock().unwrap().clone()
    }

    pub fn check_action(&self, trigger_type: &str, content: &str) -> Vec<Rule> {
        let rules_opt = self.get_rules();
        if rules_opt.is_none() {
            return Vec::new();
        }
        
        let manifest = rules_opt.unwrap();
        let mut violations = Vec::new();
        
        for rule in manifest.non_negotiables {
            // Basic trigger match
            if rule.trigger == trigger_type || rule.trigger == "always" {
                // If it's NN-NO-UNRELATED-CHANGES and content matches rewrite patterns
                if rule.id == "NN-NO-UNRELATED-CHANGES" && (content.contains("while I was here") || content.contains("refactored unrelated")) {
                    violations.push(rule.clone());
                }
                // If it's NN-TESTS-SACRED and content deletes tests
                else if rule.id == "NN-TESTS-SACRED" && (content.contains("delete test") || content.contains("remove test")) {
                    violations.push(rule.clone());
                }
                // Fallback: simple trigger-matching rule warning
                else if trigger_type == "before-edit" || trigger_type == "before-final-response" {
                    violations.push(rule.clone());
                }
            }
        }
        
        violations
    }
}
