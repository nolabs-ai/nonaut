use std::{fs, path::Path};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tree_sitter::{Language, Node, Parser};

const MAX_ITEMS: usize = 500;

pub(super) fn inspect(workspace: &Path, arguments: &Value) -> Result<String> {
    let path = super::requested_path(workspace, super::required_string(arguments, "path")?)?;
    let metadata = fs::metadata(&path).context("could not inspect file")?;
    if !metadata.is_file() {
        bail!("path is not a regular file");
    }
    if metadata.len() > super::MAX_FILE_BYTES {
        bail!("file exceeds the 2 MiB inspection limit");
    }
    let source = fs::read_to_string(&path).context("file is not valid UTF-8 text")?;
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let (language, family) = language(extension)
        .with_context(|| format!("unsupported code extension `.{extension}`"))?;
    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .context("could not initialize code parser")?;
    let tree = parser
        .parse(&source, None)
        .context("could not parse code")?;
    let mut items = Vec::new();
    collect(tree.root_node(), source.as_bytes(), family, 0, &mut items);
    if items.is_empty() {
        return Ok("No declarations found.".to_owned());
    }
    if items.len() == MAX_ITEMS {
        items.push("… index truncated by Nonaut".to_owned());
    }
    Ok(super::truncate(&items.join("\n")))
}

#[derive(Clone, Copy)]
enum Family {
    Rust,
    Python,
    JavaScript,
    Go,
}

fn language(extension: &str) -> Option<(Language, Family)> {
    match extension {
        "rs" => Some((tree_sitter_rust::LANGUAGE.into(), Family::Rust)),
        "py" | "pyi" => Some((tree_sitter_python::LANGUAGE.into(), Family::Python)),
        "js" | "jsx" | "mjs" | "cjs" => {
            Some((tree_sitter_javascript::LANGUAGE.into(), Family::JavaScript))
        }
        "ts" => Some((
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Family::JavaScript,
        )),
        "tsx" => Some((
            tree_sitter_typescript::LANGUAGE_TSX.into(),
            Family::JavaScript,
        )),
        "go" => Some((tree_sitter_go::LANGUAGE.into(), Family::Go)),
        _ => None,
    }
}

fn collect(node: Node<'_>, source: &[u8], family: Family, depth: usize, output: &mut Vec<String>) {
    if output.len() >= MAX_ITEMS {
        return;
    }
    let declaration = is_declaration(family, node.kind());
    if declaration {
        let start = node.start_position().row + 1;
        let end = node.end_position().row + 1;
        let signature = node
            .utf8_text(source)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .trim();
        let signature = if signature.len() > 180 {
            let boundary = signature.floor_char_boundary(180);
            format!("{}…", &signature[..boundary])
        } else {
            signature.to_owned()
        };
        output.push(format!(
            "{}{}-{end}  {signature}",
            "  ".repeat(depth.min(6)),
            start
        ));
    }
    let child_depth = depth + usize::from(declaration);
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect(child, source, family, child_depth, output);
        if output.len() >= MAX_ITEMS {
            break;
        }
    }
}

fn is_declaration(family: Family, kind: &str) -> bool {
    match family {
        Family::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "impl_item"
                | "trait_item"
                | "mod_item"
                | "type_item"
                | "const_item"
                | "static_item"
                | "macro_definition"
        ),
        Family::Python => matches!(kind, "function_definition" | "class_definition"),
        Family::JavaScript => matches!(
            kind,
            "function_declaration"
                | "generator_function_declaration"
                | "class_declaration"
                | "method_definition"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
        ),
        Family::Go => matches!(
            kind,
            "function_declaration"
                | "method_declaration"
                | "type_declaration"
                | "const_declaration"
                | "var_declaration"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_index_keeps_structure_and_line_ranges() {
        let source = b"struct App {}\nimpl App {\n fn run(&self) {}\n}\n";
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let mut output = Vec::new();
        collect(tree.root_node(), source, Family::Rust, 0, &mut output);
        assert_eq!(output[0], "1-1  struct App {}");
        assert_eq!(output[1], "2-4  impl App {");
        assert_eq!(output[2], "  3-3  fn run(&self) {}");
    }
}
