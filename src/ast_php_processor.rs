//! AST-based PHP processor using tree-sitter-php
//!
//! This module provides robust PHP execution capabilities by parsing PHP code
//! into an Abstract Syntax Tree (AST) using tree-sitter-php and then interpreting it.

use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use async_recursion::async_recursion;
use anyhow::{Result, anyhow};
use tracing::{debug, warn};
use tree_sitter::{Parser, Node};
use tree_sitter_php;
use regex::Regex;
use tokio::fs;
use reqwest::Client;
use serde_json::Value as JsonValue;

/// AST-based PHP processor using tree-sitter-php
pub struct AstPhpProcessor {
    parser: Parser,
    source_code: String,
    global_variables: HashMap<String, PhpValue>,
    variables: HashMap<String, PhpValue>,
    superglobals: HashMap<String, HashMap<String, String>>,
    response_status: u16,
    response_headers: HashMap<String, String>,
    response_body_override: Option<String>,
    current_template_path: Option<PathBuf>,
    root_dir: Option<PathBuf>,
    http_client: Client,
    included_files: HashSet<PathBuf>,
    output_buffers: Vec<String>,
    side_effect_output: Option<String>,
}

impl AstPhpProcessor {
    /// Create a new AST-based PHP processor
    pub fn new() -> Result<Self> {
        debug!("Initializing AST-based PHP processor using tree-sitter-php");

        let mut parser = Parser::new();
        let language = tree_sitter_php::LANGUAGE_PHP;
        parser
            .set_language(&language.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        Ok(Self {
            parser,
            source_code: String::new(),
            global_variables: HashMap::new(),
            variables: HashMap::new(),
            superglobals: HashMap::new(),
            response_status: 200,
            response_headers: HashMap::new(),
            response_body_override: None,
            current_template_path: None,
            root_dir: None,
            http_client: Client::new(),
            included_files: HashSet::new(),
            output_buffers: Vec::new(),
            side_effect_output: None,
        })
    }

    /// Execute PHP code with environment variables using AST parsing
    pub async fn execute_php(
        &mut self,
        php_code: &str,
        get_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
        template_path: &Path,
        root_dir: &Path,
    ) -> Result<PhpExecution> {
        debug!("Executing PHP code with AST processor");

        self.reset_request_state(template_path, root_dir);

        // Set up superglobals
        self.superglobals.insert("_GET".to_string(), get_params.clone());
        self.superglobals.insert("_POST".to_string(), post_params.clone());
        self.superglobals.insert("_SERVER".to_string(), server_vars.clone());

        let mut request_params = get_params.clone();
        request_params.extend(post_params.clone());
        self.superglobals.insert("_REQUEST".to_string(), request_params);

        // Prepare the output buffer
        let mut output = String::new();

        // Process the PHP code
        self.process_php_code(php_code, &mut output).await?;

        let body = self.response_body_override.clone().unwrap_or(output);
        Ok(PhpExecution {
            body,
            status: self.response_status,
            headers: self.response_headers.clone(),
        })
    }

    /// Process PHP code and return output
    async fn process_php_code(&mut self, code: &str, output: &mut String) -> Result<String> {
        let mut current_pos = 0;

        // Find PHP tags (both <?php ... ?> and <?= ... ?>)
        let php_tag_regex = Regex::new(r"(?s)<\?(php|=)?(.*?)\?>").unwrap();

        let mut found_tag = false;

        for cap in php_tag_regex.captures_iter(code) {
            found_tag = true;
            let full_match = cap.get(0).unwrap();
            let tag_kind = cap.get(1).map(|m| m.as_str()).unwrap_or("");
            let mut php_code = cap.get(2).unwrap().as_str().to_string();

            if tag_kind == "=" {
                php_code = format!("echo {};", php_code.trim());
            }

            // Add HTML content before PHP tag
            if full_match.start() > current_pos {
                output.push_str(&code[current_pos..full_match.start()]);
            }

            // Process PHP code
            let php_output = self.execute_php_ast(&php_code).await?;
            output.push_str(&php_output);

            current_pos = full_match.end();
        }

        // If no PHP tags were found but code contains a PHP open tag without closing,
        // treat the whole file as PHP (common when the closing tag is omitted).
        if !found_tag && code.contains("<?php") {
            let php_output = self.execute_php_ast_php_only(code).await?;
            return Ok(php_output);
        }

        // Add remaining HTML content
        if current_pos < code.len() {
            output.push_str(&code[current_pos..]);
        }

        Ok(output.clone())
    }

    /// Execute PHP code using AST parsing
    async fn execute_php_ast(&mut self, php_code: &str) -> Result<String> {
        // Wrap the code in PHP tags if not already present
        let wrapped_code = if php_code.trim_start().starts_with("<?php") {
            php_code.to_string()
        } else {
            format!("<?php {} ?>", php_code)
        };

        self.source_code = wrapped_code.clone();

        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        // Parse the PHP code into AST
        let tree = self.parser.parse(&wrapped_code, None)
            .ok_or_else(|| anyhow!("Failed to parse PHP code"))?;

        if tree.root_node().has_error() {
            warn!("AST parse has errors");
            debug!("AST tree: {}", tree.root_node().to_sexp());
        }

        // Interpret the AST
        let mut output = String::new();
        let flow = self.process_node(tree.root_node(), &mut output).await?;
        if let ControlFlow::Break(()) = flow {
            if output.trim().is_empty() {
                warn!("AST output empty; may contain unsupported constructs");
                debug!("AST tree: {}", tree.root_node().to_sexp());
            }
            debug!("AST output length: {}", output.len());
            return Ok(output);
        }

        if output.trim().is_empty() {
            warn!("AST output empty; may contain unsupported constructs");
            debug!("AST tree: {}", tree.root_node().to_sexp());
        }
        debug!("AST output length: {}", output.len());
        Ok(output)
    }

    async fn execute_php_ast_php_only(&mut self, php_code: &str) -> Result<String> {
        let mut code = php_code.to_string();
        if let Some(pos) = code.find("<?php") {
            code = code[(pos + 5)..].to_string();
        }
        if let Some(pos) = code.find("?>") {
            code = code[..pos].to_string();
        }

        let wrapped_code = format!("<?php {} ?>", code);

        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        self.source_code = wrapped_code.clone();
        let tree = self.parser.parse(&wrapped_code, None)
            .ok_or_else(|| anyhow!("Failed to parse PHP code"))?;

        if tree.root_node().has_error() {
            warn!("AST parse has errors");
            debug!("AST tree: {}", tree.root_node().to_sexp());
        }

        if tree.root_node().has_error() {
            warn!("PHP-only parse has errors");
        }

        let mut output = String::new();
        let flow = self.process_node(tree.root_node(), &mut output).await?;
        if let ControlFlow::Break(()) = flow {
            if output.trim().is_empty() {
                debug!("PHP fallback tree: {}", tree.root_node().to_sexp());
            }
            debug!("PHP fallback output length: {}", output.len());
            // Restore full PHP grammar for subsequent parses
            self.parser
                .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
                .map_err(|_| anyhow!("Error loading PHP grammar"))?;
            return Ok(output);
        }

        if output.trim().is_empty() {
            debug!("PHP fallback tree: {}", tree.root_node().to_sexp());
        }
        debug!("PHP fallback output length: {}", output.len());
        // Restore full PHP grammar for subsequent parses
        self.parser
            .set_language(&tree_sitter_php::LANGUAGE_PHP.into())
            .map_err(|_| anyhow!("Error loading PHP grammar"))?;

        Ok(output)
    }


    /// Process a node in the AST
    #[async_recursion]
    async fn process_node(&mut self, node: Node<'async_recursion>, output: &mut String) -> Result<ControlFlow<()>> {
        match node.kind() {
            "text" | "inline_html" => {
                let text = {
                    let source = self.source_code.as_bytes();
                    node.utf8_text(source)
                        .map_err(|_| anyhow!("Failed to get text"))?
                };
                self.append_output(output, &text.to_string());
            }
            "echo_statement" => {
                // Handle echo statements (argument_list or direct expressions)
                let mut handled = false;
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "argument_list" {
                        self.process_argument_list(child, output).await?;
                        handled = true;
                    } else {
                        let value = self.evaluate_expression(child).await?;
                        if let Some(out) = self.take_side_effect_output() {
                            self.append_output(output, &out);
                        }
                        self.append_output(output, &value.as_string());
                        handled = true;
                    }
                }
                if !handled {
                    if let Some(child) = node.named_child(0) {
                        let value = self.evaluate_expression(child).await?;
                        if let Some(out) = self.take_side_effect_output() {
                            self.append_output(output, &out);
                        }
                        self.append_output(output, &value.as_string());
                    }
                }
            }
            "return_statement" => {
                let return_value = node.child_by_field_name("argument")
                    .or_else(|| node.child_by_field_name("value"))
                    .or_else(|| node.named_child(0));
                if let Some(value_node) = return_value {
                    let value = self.evaluate_expression(value_node).await?;
                    self.response_body_override = Some(value.as_string());
                }
                return Ok(ControlFlow::Break(()));
            }
            "assignment_expression" => {
                // Handle variable assignments
                let left = node.child_by_field_name("left");
                let right = node.child_by_field_name("right");
                if let (Some(left), Some(right)) = (left, right) {
                    if let Some(var_name) = self.get_identifier(left) {
                        let value = self.evaluate_expression(right).await?;
                        self.variables.insert(var_name, value);
                    }
                }
            }
            "variable" => {
                let value = self.evaluate_expression(node).await?;
                self.append_output(output, &value.as_string());
            }
            "function_call_expression" => {
                let _ = self.evaluate_expression(node).await?;
                if let Some(out) = self.take_side_effect_output() {
                    self.append_output(output, &out);
                }
            }
            "expression_statement" => {
                if let Some(child) = node.named_child(0) {
                    let value = self.evaluate_expression(child).await?;
                    if let Some(out) = self.take_side_effect_output() {
                        self.append_output(output, &out);
                    } else {
                        let rendered = value.as_string();
                        if !rendered.is_empty() {
                            self.append_output(output, &rendered);
                        }
                    }
                }
            }
            "foreach_statement" => {
                let mut collection_node = node.child_by_field_name("collection")
                    .or_else(|| node.child_by_field_name("value"));

                if collection_node.is_none() {
                    for child in node.named_children(&mut node.walk()) {
                        if child.kind() == "array_creation_expression" {
                            collection_node = Some(child);
                            break;
                        }
                    }
                }

                let mut var_nodes = Vec::new();
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "variable" || child.kind() == "variable_name" {
                        var_nodes.push(child);
                    }
                }

                let body = node.child_by_field_name("body")
                    .or_else(|| node.child_by_field_name("statement"))
                    .or_else(|| node.named_children(&mut node.walk()).last());

                if let (Some(collection), Some(body)) = (collection_node, body) {
                    let collection_value = self.evaluate_expression(collection).await?;
                    if let PhpValue::Array(items) = collection_value {
                        for (index, item) in items.iter().enumerate() {
                            if var_nodes.len() >= 1 {
                                if let Some(value_name) = self.get_identifier(var_nodes[var_nodes.len() - 1]) {
                                    let value = match item {
                                        PhpArrayItem::KeyValue(_, value) => value.clone(),
                                        PhpArrayItem::Value(value) => value.clone(),
                                    };
                                    self.variables.insert(value_name, value);
                                }
                            }
                            if var_nodes.len() >= 2 {
                                if let Some(key_name) = self.get_identifier(var_nodes[0]) {
                                    let key_value = match item {
                                        PhpArrayItem::KeyValue(key, _) => PhpValue::String(key.clone()),
                                        PhpArrayItem::Value(_) => PhpValue::String(index.to_string()),
                                    };
                                    self.variables.insert(key_name, key_value);
                                }
                            }

                            let _ = self.process_node(body, output).await?;
                        }
                    }
                }
            }

            "include_expression" | "require_expression" | "include_once_expression" | "require_once_expression" => {
                let is_once = node.kind().contains("_once");
                let target = node.child_by_field_name("path")
                    .or_else(|| node.child_by_field_name("argument"))
                    .or_else(|| node.named_child(0));
                if let Some(target) = target {
                    let value = self.evaluate_expression(target).await?;
                    let include_output = self.include_file(&value.as_string(), is_once).await?;
                    self.append_output(output, &include_output);
                }
            }
            _ => {
                // Recursively process child nodes
                for child in node.named_children(&mut node.walk()) {
                    let flow = self.process_node(child, output).await?;
                    if let ControlFlow::Break(()) = flow {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }

    /// Process an argument list (used in echo statements)
    #[async_recursion]
    async fn process_argument_list(&mut self, node: Node<'async_recursion>, output: &mut String) -> Result<()> {
        for child in node.named_children(&mut node.walk()) {
            if child.kind() == "string" {
                // Handle string literals
                let text = child.utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                // Remove surrounding quotes
                let trimmed = text.trim_matches(|c| c == '\'' || c == '"');
                self.append_output(output, &trimmed.to_string());
            } else if child.kind() == "parenthesized_expression" {
                // Process nested expressions
                let _ = self.process_node(child, output).await?;
            } else {
                // Handle other expressions (variables, functions)
                let value = self.evaluate_expression(child).await?;
                if let Some(out) = self.take_side_effect_output() {
                    self.append_output(output, &out);
                }
                self.append_output(output, &value.as_string());
            }
        }
        Ok(())
    }

    /// Evaluate an expression node to a string value
    fn unescape_sequence(&self, raw: &str) -> String {
        match raw {
            "\\n" => "\n".to_string(),
            "\\r" => "\r".to_string(),
            "\\t" => "\t".to_string(),
            "\\\\" => "\\".to_string(),
            "\\\"" => "\"".to_string(),
            _ => raw.to_string(),
        }
    }

    fn get_identifier(&self, node: Node<'_>) -> Option<String> {
        match node.kind() {
            "variable" | "variable_name" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source).ok()?;
                Some(raw.trim_start_matches('$').to_string())
            }
            "name" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source).ok()?;
                Some(raw.to_string())
            }
            _ => None,
        }
    }

    #[async_recursion]
    async fn evaluate_expression(&mut self, node: Node<'async_recursion>) -> Result<PhpValue> {
        match node.kind() {
            "encapsed_string" => {
                let mut out = String::new();
                for child in node.named_children(&mut node.walk()) {
                    let value = self.evaluate_expression(child).await?;
                    out.push_str(&value.as_string());
                }
                return Ok(PhpValue::String(out));
            }
            "string_content" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source)
                    .map_err(|_| anyhow!("Failed to get text"))?;
                return Ok(PhpValue::String(raw.to_string()));
            }
            "escape_sequence" => {
                let source = self.source_code.as_bytes();
                let raw = node.utf8_text(source)
                    .map_err(|_| anyhow!("Failed to get text"))?;
                if raw.contains("$") {
                    let cleaned = raw.trim_matches(|c| c == '{' || c == '}' || c == '$');
                    if let Some(value) = self.variables.get(cleaned) {
                        return Ok(value.clone());
                    }
                }
                return Ok(PhpValue::String(self.unescape_sequence(raw)));
            }
            "variable_name" => {
                if let Some(id) = self.get_identifier(node) {
                    if let Some(superglobal) = self.superglobals.get(&id) {
                        let mut items = Vec::new();
                        for (key, value) in superglobal {
                            items.push(PhpArrayItem::KeyValue(key.clone(), PhpValue::String(value.clone())));
                        }
                        return Ok(PhpValue::Array(items));
                    }
                    if let Some(value) = self.variables.get(&id) {
                        return Ok(value.clone());
                    }
                    return Ok(PhpValue::Null);
                }
                return Ok(PhpValue::Null);
            }
            "name" => {
                if let Some(id) = self.get_identifier(node) {
                    return Ok(PhpValue::String(id));
                }
                return Ok(PhpValue::Null);
            }
            "single_quoted_string" | "double_quoted_string" => {
                let text = node.utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                return Ok(PhpValue::String(text.trim_matches(|c| c == '\'' || c == '"').to_string()));
            }
            "string" => {
                let text = node.utf8_text(self.source_code.as_bytes())
                    .map_err(|_| anyhow!("Failed to get text"))?;
                Ok(PhpValue::String(text.trim_matches(|c| c == '\'' || c == '"').to_string()))
            }
            "integer" | "float" => Ok(node.utf8_text(self.source_code.as_bytes())
                .map_err(|_| anyhow!("Failed to get number"))?
                .to_string().into()),
            "variable" => {
                let var_name = node.utf8_text(self.source_code.as_bytes())?
                    .trim_start_matches('$').to_string();
                if let Some(value) = self.variables.get(&var_name) {
                    Ok(value.clone())
                } else if let Some(superglobal) = self.superglobals.get(&var_name) {
                    let mut items = Vec::new();
                    for (key, value) in superglobal {
                        items.push(PhpArrayItem::KeyValue(key.clone(), PhpValue::String(value.clone())));
                    }
                    Ok(PhpValue::Array(items))
                } else {
                    Ok(PhpValue::Null)
                }
            }
            "subscript_expression" => {
                let target = node.child_by_field_name("value")
                    .or_else(|| node.child_by_field_name("array"))
                    .or_else(|| node.named_child(0));
                let index = node.child_by_field_name("index")
                    .or_else(|| node.child_by_field_name("offset"))
                    .or_else(|| node.named_child(1));
                if let (Some(target), Some(index)) = (target, index) {
                    let target_name = if let Some(id) = self.get_identifier(target) {
                        id
                    } else {
                        let source = self.source_code.as_bytes();
                        target.utf8_text(source)?.to_string().trim_start_matches('$').to_string()
                    };
                    let key = self.evaluate_expression(index).await?.as_string();
                    if let Some(superglobal) = self.superglobals.get(&target_name) {
                        if let Some(value) = superglobal.get(&key) {
                            return Ok(PhpValue::String(value.clone()));
                        }
                    }
                    if let Some(PhpValue::Array(items)) = self.variables.get(&target_name) {
                        if let Ok(index) = key.parse::<usize>() {
                            if let Some(item) = items.get(index) {
                                match item {
                                    PhpArrayItem::KeyValue(_, value) => return Ok(value.clone()),
                                    PhpArrayItem::Value(value) => return Ok(value.clone()),
                                }
                            }
                        }
                        for item in items {
                            if let PhpArrayItem::KeyValue(item_key, value) = item {
                                if item_key == &key {
                                    return Ok(value.clone());
                                }
                            }
                        }
                    }
                }
                Ok(PhpValue::Null)
            }
            "binary_expression" => {
                let left = node.child_by_field_name("left").or_else(|| node.named_child(0));
                let right = node.child_by_field_name("right").or_else(|| node.named_child(1));
                let operator = {
                    let source = self.source_code.as_bytes();
                    node.child_by_field_name("operator")
                        .and_then(|op| op.utf8_text(source).ok())
                        .unwrap_or_default()
                        .to_string()
                };
                if let (Some(left), Some(right)) = (left, right) {
                    let left_val = self.evaluate_expression(left).await?;
                    let right_val = self.evaluate_expression(right).await?;
                    if operator == "." {
                        return Ok(PhpValue::String(format!("{}{}", left_val.as_string(), right_val.as_string())));
                    }
                }
                Ok(PhpValue::Null)
            }
            "text_interpolation" => {
                let mut out = String::new();
                for child in node.named_children(&mut node.walk()) {
                    let value = self.evaluate_expression(child).await?;
                    out.push_str(&value.as_string());
                }
                return Ok(PhpValue::String(out));
            }
            "parenthesized_expression" => {
                if let Some(inner) = node.named_child(0) {
                    return self.evaluate_expression(inner).await;
                }
                Ok(PhpValue::Null)
            }
            "array_creation_expression" => {
                let mut items = Vec::new();
                for child in node.named_children(&mut node.walk()) {
                    if child.kind() == "array_element_initializer" {
                        let key_node = child.child_by_field_name("key");
                        let value_node = child.child_by_field_name("value")
                            .or_else(|| child.named_child(0));
                        if let Some(value_node) = value_node {
                            let value = self.evaluate_expression(value_node).await?;
                            if let Some(key_node) = key_node {
                                let key = self.evaluate_expression(key_node).await?.as_string();
                                items.push(PhpArrayItem::KeyValue(key, value));
                            } else {
                                items.push(PhpArrayItem::Value(value));
                            }
                        }
                    }
                }
                Ok(PhpValue::Array(items))
            }
            "function_call_expression" => {
                self.evaluate_function_call(node).await
            }
            _ => {
                let mut expr_output = String::new();
                let _ = self.process_node(node, &mut expr_output).await?;
                Ok(PhpValue::String(expr_output))
            }
        }
    }

    async fn evaluate_function_call(&mut self, node: Node<'_>) -> Result<PhpValue> {
        let function = node.child_by_field_name("function")
            .ok_or_else(|| anyhow!("Function node missing"))?;
        let mut args_node = node.child_by_field_name("arguments");
        if args_node.is_none() {
            for child in node.named_children(&mut node.walk()) {
                if child.kind() == "arguments" || child.kind() == "argument_list" {
                    args_node = Some(child);
                    break;
                }
            }
        }
        let func_name = {
            let source = self.source_code.as_bytes();
            function.utf8_text(source)?.to_string()
        };

        let mut args = Vec::new();
        if let Some(args_node) = args_node {
            for child in args_node.named_children(&mut args_node.walk()) {
                if child.kind() == "argument" {
                    if let Some(inner) = child.named_child(0) {
                        args.push(self.evaluate_expression(inner).await?);
                        continue;
                    }
                }
                args.push(self.evaluate_expression(child).await?);
            }
        }

        match func_name.as_str() {
            "phpversion" => Ok(PhpValue::String("8.4.0-ast".to_string())),
            "date" => {
                let now = chrono::Utc::now();
                Ok(PhpValue::String(now.format("%Y-%m-%d %H:%M:%S").to_string()))
            }
            "header" => {
                if let Some(header_line) = args.get(0) {
                    let header_line = header_line.as_string();
                    if let Some((name, value)) = header_line.split_once(':') {
                        self.response_headers.insert(name.trim().to_string(), value.trim().to_string());
                    }
                }
                Ok(PhpValue::Null)
            }
            "response" => {
                if let Some(status) = args.get(0) {
                    if let Ok(status) = status.as_string().parse::<u16>() {
                        self.response_status = status;
                    }
                }
                if let Some(headers) = args.get(1) {
                    self.apply_header_block(headers);
                }
                if let Some(body) = args.get(2) {
                    self.response_body_override = Some(body.as_string());
                }
                Ok(PhpValue::Null)
            }
            "render" => {
                if let Some(template) = args.get(0) {
                    if let Some(rendered) = self.render_template(template, args.get(1)).await? {
                        return Ok(PhpValue::String(rendered));
                    }
                }
                Ok(PhpValue::Null)
            }
            "ob_start" => {
                self.output_buffers.push(String::new());
                Ok(PhpValue::Null)
            }
            "ob_get_clean" => {
                let buffer = self.output_buffers.pop().unwrap_or_default();
                Ok(PhpValue::String(buffer))
            }
            "file_get_contents" => {
                if let Some(target) = args.get(0) {
                    let headers = args.get(1);
                    return self.fetch_content(target, headers).await;
                }
                Ok(PhpValue::Null)
            }
            "var_dump" => {
                if args.is_empty() {
                    warn!("var_dump called with no args (parser did not capture arguments)");
                }
                let mut out = String::new();
                for (idx, arg) in args.iter().enumerate() {
                    if idx > 0 {
                        out.push('\n');
                    }
                    out.push_str(&arg.dump());
                }
                self.side_effect_output = Some(out);
                return Ok(PhpValue::Null);
            }
            "http_request" => {
                let method = args.get(0).map(|s| s.as_string()).unwrap_or("GET".to_string());
                let url = args.get(1).map(|s| s.as_string()).unwrap_or_default();
                let headers = args.get(2);
                let body = args.get(3).map(|s| s.as_string()).unwrap_or_default();
                if !url.is_empty() {
                    return self.http_request(&method, &url, headers, &body).await;
                }
                Ok(PhpValue::Null)
            }
            _ => Ok(PhpValue::Null),
        }
    }

    fn reset_request_state(&mut self, template_path: &Path, root_dir: &Path) {
        self.variables = self.global_variables.clone();
        self.superglobals.clear();
        self.response_status = 200;
        self.response_headers.clear();
        self.response_body_override = None;
        self.current_template_path = Some(template_path.to_path_buf());
        self.root_dir = Some(root_dir.to_path_buf());
        self.included_files.clear();
        self.output_buffers.clear();
        self.side_effect_output = None;
    }

    pub async fn execute_init(
        &mut self,
        php_code: &str,
        server_vars: &HashMap<String, String>,
        template_path: &Path,
        root_dir: &Path,
    ) -> Result<()> {
        self.reset_request_state(template_path, root_dir);
        self.superglobals.insert("_SERVER".to_string(), server_vars.clone());

        let mut output = String::new();
        self.process_php_code(php_code, &mut output).await?;
        self.global_variables = self.variables.clone();
        Ok(())
    }

    async fn render_template(
        &mut self,
        template: &PhpValue,
        data: Option<&PhpValue>,
    ) -> Result<Option<String>> {
        let path = self.resolve_local_path(&template.as_string())?;
        let content = fs::read_to_string(&path).await
            .map_err(|_| anyhow!("Cannot read template"))?;

        let previous_source = self.source_code.clone();
        let previous_template = self.current_template_path.clone();
        self.current_template_path = Some(path.clone());

        if let Some(data) = data {
            self.apply_data_to_vars(data);
        }

        let mut output = String::new();
        self.process_php_code(&content, &mut output).await?;
        self.source_code = previous_source;
        self.current_template_path = previous_template;
        Ok(Some(output))
    }

    async fn fetch_content(&mut self, target: &PhpValue, headers: Option<&PhpValue>) -> Result<PhpValue> {
        let target = target.as_string();
        if target.starts_with("http://") || target.starts_with("https://") {
            let mut request = self.http_client.get(&target);
            if let Some(headers) = headers {
                for (name, value) in self.extract_headers(headers) {
                    request = request.header(name, value);
                }
            }
            let response = request.send().await
                .map_err(|e| anyhow!("HTTP request failed: {}", e))?;
            let bytes = response.bytes().await
                .map_err(|e| anyhow!("Failed to read response: {}", e))?;
            return Ok(PhpValue::String(String::from_utf8_lossy(&bytes).to_string()));
        }

        let path = self.resolve_local_path(&target)?;
        let bytes = fs::read(&path).await
            .map_err(|_| anyhow!("Cannot read file"))?;
        Ok(PhpValue::String(String::from_utf8_lossy(&bytes).to_string()))
    }

    async fn http_request(
        &mut self,
        method: &str,
        url: &str,
        headers: Option<&PhpValue>,
        body: &str,
    ) -> Result<PhpValue> {
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .unwrap_or(reqwest::Method::GET);
        let mut request = self.http_client.request(method, url);
        if let Some(headers) = headers {
            for (name, value) in self.extract_headers(headers) {
                request = request.header(name, value);
            }
        }
        if !body.is_empty() {
            request = request.body(body.to_string());
        }
        let response = request.send().await
            .map_err(|e| anyhow!("HTTP request failed: {}", e))?;
        let bytes = response.bytes().await
            .map_err(|e| anyhow!("Failed to read response: {}", e))?;
        Ok(PhpValue::String(String::from_utf8_lossy(&bytes).to_string()))
    }

    fn resolve_local_path(&self, target: &str) -> Result<PathBuf> {
        let root_dir = self.root_dir.as_ref().ok_or_else(|| anyhow!("Root dir not set"))?;
        let base_dir = self.current_template_path
            .as_ref()
            .and_then(|p| p.parent())
            .unwrap_or(root_dir);

        let raw_path = if target.starts_with('/') {
            root_dir.join(target.trim_start_matches('/'))
        } else {
            base_dir.join(target)
        };

        let canonical_root = root_dir.canonicalize()
            .map_err(|_| anyhow!("Cannot canonicalize root dir"))?;
        let canonical_path = raw_path.canonicalize()
            .map_err(|_| anyhow!("Cannot resolve path"))?;

        if !canonical_path.starts_with(&canonical_root) {
            return Err(anyhow!("Path traversal attempt detected"));
        }

        Ok(canonical_path)
    }

    fn apply_header_block(&mut self, headers: &PhpValue) {
        for (name, value) in self.extract_headers(headers) {
            self.response_headers.insert(name, value);
        }
    }

    fn extract_headers(&self, headers: &PhpValue) -> Vec<(String, String)> {
        match headers {
            PhpValue::Array(items) => {
                let mut pairs = Vec::new();
                for item in items {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            pairs.push((key.clone(), value.as_string()));
                        }
                        PhpArrayItem::Value(value) => {
                            if let Some((name, value)) = value.as_string().split_once(':') {
                                pairs.push((name.trim().to_string(), value.trim().to_string()));
                            }
                        }
                    }
                }
                pairs
            }
            PhpValue::String(headers) => parse_header_block(headers),
            _ => Vec::new(),
        }
    }

    fn apply_data_to_vars(&mut self, data: &PhpValue) {
        match data {
            PhpValue::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            self.variables.insert(key.clone(), value.clone());
                        }
                        PhpArrayItem::Value(value) => {
                            self.variables.insert(index.to_string(), value.clone());
                        }
                    }
                }
            }
            PhpValue::String(json) => {
                if let Ok(json) = serde_json::from_str::<JsonValue>(json) {
                    if let Some(obj) = json.as_object() {
                        for (key, value) in obj {
                            if let Some(value) = value.as_str() {
                                self.variables.insert(key.clone(), PhpValue::String(value.to_string()));
                            } else {
                                self.variables.insert(key.clone(), PhpValue::String(value.to_string()));
                            }
                        }
                        return;
                    }
                }
                self.variables.insert("data".to_string(), PhpValue::String(json.clone()));
            }
            _ => {
                self.variables.insert("data".to_string(), data.clone());
            }
        }
    }

    fn append_output(&mut self, output: &mut String, text: &str) {
        if let Some(buffer) = self.output_buffers.last_mut() {
            buffer.push_str(text);
        } else {
            output.push_str(text);
        }
    }

    fn take_side_effect_output(&mut self) -> Option<String> {
        self.side_effect_output.take()
    }

    async fn include_file(&mut self, target: &str, once: bool) -> Result<String> {
        let path = self.resolve_local_path(target)?;
        let canonical = path.canonicalize()
            .map_err(|_| anyhow!("Cannot resolve include path"))?;

        if once && self.included_files.contains(&canonical) {
            return Ok(String::new());
        }

        let content = fs::read_to_string(&canonical).await
            .map_err(|_| anyhow!("Cannot read include file"))?;

        let previous_source = self.source_code.clone();
        let previous_template = self.current_template_path.clone();
        self.current_template_path = Some(canonical.clone());
        self.included_files.insert(canonical);

        let mut output = String::new();
        self.process_php_code(&content, &mut output).await?;

        self.source_code = previous_source;
        self.current_template_path = previous_template;
        Ok(output)
    }
}

fn parse_header_block(headers: &str) -> Vec<(String, String)> {
    headers
        .split(|c| c == '\n' || c == '\r')
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            line.split_once(':').map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        })
        .collect()
}

pub struct PhpExecution {
    pub body: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub enum PhpValue {
    String(String),
    Array(Vec<PhpArrayItem>),
    Null,
}

#[derive(Clone, Debug)]
pub enum PhpArrayItem {
    KeyValue(String, PhpValue),
    Value(PhpValue),
}

impl PhpValue {
    pub fn as_string(&self) -> String {
        match self {
            PhpValue::String(value) => value.clone(),
            PhpValue::Array(_) => "[array]".to_string(),
            PhpValue::Null => String::new(),
        }
    }

    pub fn dump(&self) -> String {
        match self {
            PhpValue::String(value) => format!("string({}) \"{}\"", value.len(), value),
            PhpValue::Array(items) => {
                let mut out = String::new();
                out.push_str(&format!("array({}) {{\n", items.len()));
                for (index, item) in items.iter().enumerate() {
                    match item {
                        PhpArrayItem::KeyValue(key, value) => {
                            out.push_str(&format!("  [\"{}\"]=>\n    {}\n", key, value.dump()));
                        }
                        PhpArrayItem::Value(value) => {
                            out.push_str(&format!("  [{}]=>\n    {}\n", index, value.dump()));
                        }
                    }
                }
                out.push('}');
                out
            }
            PhpValue::Null => "NULL".to_string(),
        }
    }
}

impl From<String> for PhpValue {
    fn from(value: String) -> Self {
        PhpValue::String(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::TempDir;
    use tokio::fs::write;

    #[tokio::test]
    async fn test_ast_php_processor_creation() {
        let processor = AstPhpProcessor::new().unwrap();
        // Ensure the parser is initialized
        assert!(processor.parser.language().is_some());
    }

    #[tokio::test]
    async fn test_ast_php_execution() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();

        let php_code = r#"<html><body><?php echo "Hello from AST PHP!"; ?></body></html>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor.execute_php(
            php_code,
            &get_params,
            &post_params,
            &server_vars,
            &template_path,
            temp_dir.path(),
        ).await.unwrap();
        assert!(result.body.contains("Hello from AST PHP!"));
    }

    #[tokio::test]
    async fn test_ast_php_inline_html() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();

        let php_code = r#"<html><body>Welcome to <?php echo "My Site"; ?></body></html>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor.execute_php(
            php_code,
            &get_params,
            &post_params,
            &server_vars,
            &template_path,
            temp_dir.path(),
        ).await.unwrap();
        assert!(result.body.contains("Welcome to My Site"));
    }

    #[tokio::test]
    async fn test_ast_php_variable_echo() {
        let mut processor = AstPhpProcessor::new().unwrap();
        let mut get_params = HashMap::new();
        get_params.insert("user".to_string(), "Alice".to_string());
        let post_params = HashMap::new();
        let mut server_vars = HashMap::new();
        server_vars.insert("SERVER_NAME".to_string(), "localhost".to_string());

        let php_code = r#"<?php $name = $_GET['user']; echo "Hello, " . $name; ?>"#;

        let temp_dir = TempDir::new().unwrap();
        let template_path = temp_dir.path().join("_index.php");
        write(&template_path, php_code).await.unwrap();

        let result = processor.execute_php(
            php_code,
            &get_params,
            &post_params,
            &server_vars,
            &template_path,
            temp_dir.path(),
        ).await.unwrap();
        assert!(result.body.contains("Hello, Alice"));
    }
}
