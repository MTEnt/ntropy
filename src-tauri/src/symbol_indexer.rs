use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use tree_sitter::{Node, Parser};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeSymbol {
    pub name: String,
    pub kind: String, // "Struct", "Enum", "Function", "Class", "Method", "Interface"
    pub line_number: usize,
    pub range_start: usize,
    pub range_end: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSymbols {
    pub relative_path: String,
    pub language: String,
    pub symbols: Vec<CodeSymbol>,
}

pub struct SymbolIndexer {
    pub workspace_root: std::sync::Mutex<PathBuf>,
}

impl SymbolIndexer {
    pub fn new<P: AsRef<Path>>(root: P) -> Self {
        Self {
            workspace_root: std::sync::Mutex::new(root.as_ref().to_path_buf()),
        }
    }

    pub fn set_workspace_root(&self, path: PathBuf) {
        let mut root = self.workspace_root.lock().unwrap();
        *root = path;
    }

    pub fn index_file(&self, relative_path: &str) -> Result<FileSymbols, String> {
        let rel_path = Path::new(relative_path);
        if rel_path.is_absolute()
            || rel_path
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            return Err("Security Violation: Path traversal or absolute path detected".to_string());
        }

        let root_path = self.workspace_root.lock().unwrap().clone();
        let full_path = root_path.join(relative_path);
        let content = fs::read_to_string(&full_path)
            .map_err(|e| format!("Failed to read file for symbol indexing: {}", e))?;

        let ext = full_path.extension().and_then(|s| s.to_str()).unwrap_or("");

        let (mut parser, language_name) = match ext {
            "rs" => {
                let mut p = Parser::new();
                let _ = p.set_language(&tree_sitter_rust::LANGUAGE.into());
                (p, "rust".to_string())
            }
            "ts" | "tsx" => {
                let mut p = Parser::new();
                let _ = p.set_language(&tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into());
                (p, "typescript".to_string())
            }
            "js" | "jsx" => {
                let mut p = Parser::new();
                let _ = p.set_language(&tree_sitter_javascript::LANGUAGE.into());
                (p, "javascript".to_string())
            }
            "py" => {
                let mut p = Parser::new();
                let _ = p.set_language(&tree_sitter_python::LANGUAGE.into());
                (p, "python".to_string())
            }
            _ => {
                return Ok(FileSymbols {
                    relative_path: relative_path.to_string(),
                    language: "unknown".to_string(),
                    symbols: Vec::new(),
                })
            }
        };

        let tree = parser
            .parse(&content, None)
            .ok_or_else(|| "Failed to parse file syntax tree".to_string())?;

        let root_node = tree.root_node();
        let mut symbols = Vec::new();
        let source_bytes = content.as_bytes();

        self.extract_symbols_recursive(root_node, source_bytes, &language_name, &mut symbols);

        Ok(FileSymbols {
            relative_path: relative_path.to_string(),
            language: language_name,
            symbols,
        })
    }

    fn extract_symbols_recursive(
        &self,
        node: Node,
        source: &[u8],
        language: &str,
        acc: &mut Vec<CodeSymbol>,
    ) {
        let node_type = node.kind();
        let mut symbol_opt = None;

        match language {
            "rust" => {
                match node_type {
                    "struct_item" => {
                        if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                            symbol_opt = Some(("Struct".to_string(), name));
                        }
                    }
                    "enum_item" => {
                        if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                            symbol_opt = Some(("Enum".to_string(), name));
                        }
                    }
                    "function_item" => {
                        if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                            symbol_opt = Some(("Function".to_string(), name));
                        }
                    }
                    "impl_item" => {
                        // Impl blocks
                        if let Some(trait_node) = node.child_by_field_name("trait") {
                            if let Ok(name) = trait_node.utf8_text(source) {
                                symbol_opt = Some(("Impl".to_string(), format!("impl {}", name)));
                            }
                        } else if let Some(type_node) = node.child_by_field_name("type") {
                            if let Ok(name) = type_node.utf8_text(source) {
                                symbol_opt = Some(("Impl".to_string(), format!("impl {}", name)));
                            }
                        }
                    }
                    _ => {}
                }
            }
            "typescript" | "javascript" => match node_type {
                "class_declaration" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Class".to_string(), name));
                    }
                }
                "function_declaration" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Function".to_string(), name));
                    }
                }
                "method_definition" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Method".to_string(), name));
                    }
                }
                "interface_declaration" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Interface".to_string(), name));
                    }
                }
                _ => {}
            },
            "python" => match node_type {
                "class_definition" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Class".to_string(), name));
                    }
                }
                "function_definition" => {
                    if let Some(name) = self.get_node_child_by_field(node, "name", source) {
                        symbol_opt = Some(("Function".to_string(), name));
                    }
                }
                _ => {}
            },
            _ => {}
        }

        if let Some((kind, name)) = symbol_opt {
            let start_point = node.start_position();
            acc.push(CodeSymbol {
                name,
                kind,
                line_number: start_point.row + 1, // 1-indexed
                range_start: node.start_byte(),
                range_end: node.end_byte(),
            });
        }

        // Recurse into children
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            self.extract_symbols_recursive(child, source, language, acc);
        }
    }

    fn get_node_child_by_field(
        &self,
        node: Node,
        field_name: &str,
        source: &[u8],
    ) -> Option<String> {
        node.child_by_field_name(field_name)
            .and_then(|child| child.utf8_text(source).ok().map(|s| s.to_string()))
    }
}
