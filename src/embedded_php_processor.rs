//! Simple PHP processor using regex-based parsing
//!
//! This module provides a lightweight PHP execution capability by parsing and executing
//! basic PHP constructs without requiring external PHP binaries or complex embedding.
//! It supports common PHP features like echo, variables, basic functions, and superglobals.

use std::collections::HashMap;
use anyhow::Result;
use tracing::debug;
use regex::Regex;
use chrono::{DateTime, Utc};

/// Simple embedded PHP processor
pub struct EmbeddedPhpProcessor {
    /// Built-in PHP functions
    builtin_functions: HashMap<String, fn(&[String]) -> String>,
}

impl EmbeddedPhpProcessor {
    /// Create a new embedded PHP processor
    pub fn new() -> Result<Self> {
        debug!("Initializing simple PHP processor");
        
        let mut builtin_functions = HashMap::new();
        
        // Add built-in PHP functions
        builtin_functions.insert("phpversion".to_string(), Self::php_version as fn(&[String]) -> String);
        builtin_functions.insert("date".to_string(), Self::php_date as fn(&[String]) -> String);
        builtin_functions.insert("time".to_string(), Self::php_time as fn(&[String]) -> String);
        builtin_functions.insert("strlen".to_string(), Self::php_strlen as fn(&[String]) -> String);
        builtin_functions.insert("strtoupper".to_string(), Self::php_strtoupper as fn(&[String]) -> String);
        builtin_functions.insert("strtolower".to_string(), Self::php_strtolower as fn(&[String]) -> String);
        builtin_functions.insert("htmlspecialchars".to_string(), Self::php_htmlspecialchars as fn(&[String]) -> String);
        
        debug!("Simple PHP processor initialized with {} built-in functions", builtin_functions.len());
        
        Ok(Self { builtin_functions })
    }
    
    /// Execute PHP code with environment variables
    pub fn execute_php(
        &self,
        php_code: &str,
        get_params: &HashMap<String, String>,
        post_params: &HashMap<String, String>,
        server_vars: &HashMap<String, String>,
    ) -> Result<String> {
        debug!("Executing PHP code with simple processor");
        
        let mut context = PhpContext::new();
        
        // Set up superglobals
        context.set_superglobal("_GET", get_params.clone());
        context.set_superglobal("_POST", post_params.clone());
        context.set_superglobal("_SERVER", server_vars.clone());
        
        // Combine GET and POST for $_REQUEST
        let mut request_params = get_params.clone();
        request_params.extend(post_params.clone());
        context.set_superglobal("_REQUEST", request_params);
        
        // Process the PHP code
        let output = self.process_php_code(php_code, &mut context)?;
        
        debug!("PHP execution completed, output length: {}", output.len());
        Ok(output)
    }
    
    /// Process PHP code and return output
    fn process_php_code(&self, code: &str, context: &mut PhpContext) -> Result<String> {
        let mut output = String::new();
        let mut current_pos = 0;
        
        // Find PHP tags (both <?php and short tags)
        let php_tag_regex = Regex::new(r"<\?(?:php)?\s*(.*?)\?>").unwrap();
        
        for cap in php_tag_regex.captures_iter(code) {
            let match_obj = cap.get(0).unwrap();
            
            // Add HTML content before PHP tag
            if match_obj.start() > current_pos {
                output.push_str(&code[current_pos..match_obj.start()]);
            }
            
            // Process PHP code
            let php_code = cap.get(1).unwrap().as_str().trim();
            let php_output = self.execute_php_statements(php_code, context)?;
            output.push_str(&php_output);
            
            current_pos = match_obj.end();
        }
        
        // Add remaining HTML content
        if current_pos < code.len() {
            output.push_str(&code[current_pos..]);
        }
        
        Ok(output)
    }
    
    /// Execute PHP statements
    fn execute_php_statements(&self, code: &str, context: &mut PhpContext) -> Result<String> {
        let mut output = String::new();
        
        // Handle control structures first
        if let Some(result) = self.handle_control_structures(code, context)? {
            return Ok(result);
        }
        
        // Split into statements (simplified)
        let statements: Vec<&str> = code.split(';').collect();
        
        for statement in statements {
            let statement = statement.trim();
            if statement.is_empty() {
                continue;
            }
            
            // Handle echo statements
            if let Some(echo_content) = self.parse_echo_statement(statement)? {
                let evaluated = self.evaluate_expression(&echo_content, context)?;
                output.push_str(&evaluated);
            }
            // Handle variable assignments
            else if let Some((var_name, var_value)) = self.parse_assignment(statement)? {
                let evaluated = self.evaluate_expression(&var_value, context)?;
                context.set_variable(var_name, evaluated);
            }
            // Handle header() function calls
            else if statement.starts_with("header(") {
                // Ignore header calls for now (they would be handled by the web server)
                debug!("Ignoring header() call: {}", statement);
            }
        }
        
        Ok(output)
    }
    
    /// Parse echo statement
    fn parse_echo_statement(&self, statement: &str) -> Result<Option<String>> {
        let echo_regex = Regex::new(r"^\s*echo\s+(.+)$").unwrap();
        
        if let Some(cap) = echo_regex.captures(statement) {
            Ok(Some(cap.get(1).unwrap().as_str().to_string()))
        } else {
            Ok(None)
        }
    }
    
    /// Parse variable assignment
    fn parse_assignment(&self, statement: &str) -> Result<Option<(String, String)>> {
        let assign_regex = Regex::new(r"^\s*\$(\w+)\s*=\s*(.+)$").unwrap();
        
        if let Some(cap) = assign_regex.captures(statement) {
            let var_name = cap.get(1).unwrap().as_str().to_string();
            let var_value = cap.get(2).unwrap().as_str().to_string();
            Ok(Some((var_name, var_value)))
        } else {
            Ok(None)
        }
    }
    
    /// Evaluate PHP expression
    fn evaluate_expression(&self, expr: &str, context: &PhpContext) -> Result<String> {
        let expr = expr.trim();
        
        // Handle null coalescing operator (??)
        if expr.contains("??") {
            let parts: Vec<&str> = expr.split("??").collect();
            if parts.len() == 2 {
                let left = self.evaluate_expression(parts[0].trim(), context)?;
                if !left.is_empty() && left != "Unknown" && left != "None" {
                    return Ok(left);
                }
                return self.evaluate_expression(parts[1].trim(), context);
            }
        }
        
        // Handle string literals
        if (expr.starts_with('"') && expr.ends_with('"')) || (expr.starts_with('\'') && expr.ends_with('\'')) {
            return Ok(expr[1..expr.len()-1].to_string());
        }
        
        // Handle variables
        if expr.starts_with('$') && !expr.contains('[') {
            let var_name = &expr[1..];
            if let Some(value) = context.get_variable(var_name) {
                return Ok(value);
            }
        }
        
        // Handle superglobal array access
        if let Some(value) = self.parse_superglobal_access(expr, context)? {
            return Ok(value);
        }
        
        // Handle function calls
        if let Some(result) = self.parse_function_call(expr, context)? {
            return Ok(result);
        }
        
        // Default: return as string
        Ok(expr.to_string())
    }
    
    /// Parse superglobal array access like $_GET['key']
    fn parse_superglobal_access(&self, expr: &str, context: &PhpContext) -> Result<Option<String>> {
        let superglobal_regex = Regex::new(r#"^\$(_[A-Z]+)\[['"](.*)['"]]\]?$"#).unwrap();
        
        if let Some(cap) = superglobal_regex.captures(expr) {
            let superglobal_name = cap.get(1).unwrap().as_str();
            let key = cap.get(2).unwrap().as_str();
            
            if let Some(superglobal) = context.get_superglobal(superglobal_name) {
                if let Some(value) = superglobal.get(key) {
                    return Ok(Some(value.clone()));
                }
            }
            // Return "Unknown" for missing superglobal values
            return Ok(Some("Unknown".to_string()));
        }
        
        Ok(None)
    }
    
    /// Parse function call
    fn parse_function_call(&self, expr: &str, context: &PhpContext) -> Result<Option<String>> {
        let func_regex = Regex::new(r"^(\w+)\((.*?)\)$").unwrap();
        
        if let Some(cap) = func_regex.captures(expr) {
            let func_name = cap.get(1).unwrap().as_str();
            let args_str = cap.get(2).unwrap().as_str();
            
            // Parse arguments (simplified)
            let args: Vec<String> = if args_str.trim().is_empty() {
                Vec::new()
            } else {
                args_str.split(',').map(|arg| {
                    self.evaluate_expression(arg.trim(), context).unwrap_or_default()
                }).collect()
            };
            
            if let Some(func) = self.builtin_functions.get(func_name) {
                return Ok(Some(func(&args)));
            }
        }
        
        Ok(None)
    }
    
    // Built-in PHP functions
    fn php_version(_args: &[String]) -> String {
        "8.4.0-simple".to_string()
    }
    
    fn php_date(args: &[String]) -> String {
        if args.is_empty() {
            return "Y-m-d H:i:s".to_string();
        }
        
        let format = &args[0];
        let now: DateTime<Utc> = Utc::now();
        
        // Simple date formatting (basic implementation)
        match format.as_str() {
            "Y-m-d H:i:s" => now.format("%Y-%m-%d %H:%M:%S").to_string(),
            "Y-m-d" => now.format("%Y-%m-%d").to_string(),
            "H:i:s" => now.format("%H:%M:%S").to_string(),
            _ => now.format("%Y-%m-%d %H:%M:%S").to_string(),
        }
    }
    
    fn php_time(_args: &[String]) -> String {
        let now: DateTime<Utc> = Utc::now();
        now.timestamp().to_string()
    }
    
    fn php_strlen(args: &[String]) -> String {
        if args.is_empty() {
            return "0".to_string();
        }
        args[0].len().to_string()
    }
    
    fn php_strtoupper(args: &[String]) -> String {
        if args.is_empty() {
            return String::new();
        }
        args[0].to_uppercase()
    }
    
    fn php_strtolower(args: &[String]) -> String {
        if args.is_empty() {
            return String::new();
        }
        args[0].to_lowercase()
    }
    
    fn php_htmlspecialchars(args: &[String]) -> String {
        if args.is_empty() {
            return String::new();
        }
        
        args[0]
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;")
            .replace('\'', "&#039;")
    }
    
    /// Handle control structures like if/endif, foreach/endforeach
    fn handle_control_structures(&self, code: &str, context: &mut PhpContext) -> Result<Option<String>> {
        // Handle if statements
        if let Some(result) = self.handle_if_statement(code, context)? {
            return Ok(Some(result));
        }
        
        // Handle foreach loops
        if let Some(result) = self.handle_foreach_loop(code, context)? {
            return Ok(Some(result));
        }
        
        Ok(None)
    }
    
    /// Handle if statements with endif
    fn handle_if_statement(&self, code: &str, context: &mut PhpContext) -> Result<Option<String>> {
        let if_regex = Regex::new(r"(?s)if\s*\(\s*(.+?)\s*\)\s*:(.*?)endif").unwrap();
        
        if let Some(cap) = if_regex.captures(code) {
            let condition = cap.get(1).unwrap().as_str();
            let body = cap.get(2).unwrap().as_str();
            
            // Evaluate condition (simplified)
            let condition_result = self.evaluate_condition(condition, context)?;
            
            if condition_result {
                return Ok(Some(self.execute_php_statements(body, context)?));
            } else {
                return Ok(Some(String::new()));
            }
        }
        
        Ok(None)
    }
    
    /// Handle foreach loops with endforeach
    fn handle_foreach_loop(&self, code: &str, context: &mut PhpContext) -> Result<Option<String>> {
        let foreach_regex = Regex::new(r"(?s)foreach\s*\(\s*(.+?)\s+as\s+(.+?)\s*\)\s*:(.*?)endforeach").unwrap();
        
        if let Some(cap) = foreach_regex.captures(code) {
            let array_expr = cap.get(1).unwrap().as_str();
            let var_expr = cap.get(2).unwrap().as_str();
            let body = cap.get(3).unwrap().as_str();
            
            let mut output = String::new();
            
            // Handle foreach over superglobals
            if let Some(array_data) = self.get_array_data(array_expr, context)? {
                // Parse variable expression (key => value or just value)
                if var_expr.contains("=>") {
                    let parts: Vec<&str> = var_expr.split("=>").collect();
                    if parts.len() == 2 {
                        let key_var = parts[0].trim().trim_start_matches('$');
                        let value_var = parts[1].trim().trim_start_matches('$');
                        
                        for (key, value) in array_data {
                            context.set_variable(key_var.to_string(), key);
                            context.set_variable(value_var.to_string(), value);
                            output.push_str(&self.execute_php_statements(body, context)?);
                        }
                    }
                } else {
                    let value_var = var_expr.trim().trim_start_matches('$');
                    for (_, value) in array_data {
                        context.set_variable(value_var.to_string(), value);
                        output.push_str(&self.execute_php_statements(body, context)?);
                    }
                }
            }
            
            return Ok(Some(output));
        }
        
        Ok(None)
    }
    
    /// Get array data from expression
    fn get_array_data(&self, expr: &str, context: &PhpContext) -> Result<Option<Vec<(String, String)>>> {
        let expr = expr.trim();
        
        // Handle superglobals like $_GET, $_POST, etc.
        if expr.starts_with('$') {
            let var_name = &expr[1..];
            debug!("Looking for superglobal: {}", var_name);
            
            if let Some(superglobal) = context.get_superglobal(var_name) {
                let mut result = Vec::new();
                for (key, value) in superglobal {
                    result.push((key.clone(), value.clone()));
                }
                debug!("Found {} items in superglobal {}", result.len(), var_name);
                return Ok(Some(result));
            } else {
                debug!("Superglobal {} not found", var_name);
            }
        }
        
        Ok(None)
    }
    
    /// Evaluate condition (simplified)
    fn evaluate_condition(&self, condition: &str, context: &PhpContext) -> Result<bool> {
        let condition = condition.trim();
        
        // Handle !empty() function
        if condition.starts_with("!empty(") && condition.ends_with(")") {
            let inner = &condition[8..condition.len()-1];
            let value = self.evaluate_expression(inner, context)?;
            return Ok(!value.is_empty() && value != "0" && value != "false");
        }
        
        // Handle empty() function
        if condition.starts_with("empty(") && condition.ends_with(")") {
            let inner = &condition[7..condition.len()-1];
            let value = self.evaluate_expression(inner, context)?;
            return Ok(value.is_empty() || value == "0" || value == "false");
        }
        
        // Default: assume true for now
        Ok(true)
    }
}

/// PHP execution context
struct PhpContext {
    variables: HashMap<String, String>,
    superglobals: HashMap<String, HashMap<String, String>>,
}

impl PhpContext {
    fn new() -> Self {
        Self {
            variables: HashMap::new(),
            superglobals: HashMap::new(),
        }
    }
    
    fn set_variable(&mut self, name: String, value: String) {
        self.variables.insert(name, value);
    }
    
    fn get_variable(&self, name: &str) -> Option<String> {
        self.variables.get(name).cloned()
    }
    
    fn set_superglobal(&mut self, name: &str, values: HashMap<String, String>) {
        self.superglobals.insert(name.to_string(), values);
    }
    
    fn get_superglobal(&self, name: &str) -> Option<&HashMap<String, String>> {
        self.superglobals.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_simple_php_processor_creation() {
        let processor = EmbeddedPhpProcessor::new().unwrap();
        assert!(!processor.builtin_functions.is_empty());
    }
    
    #[test]
    fn test_simple_php_execution() {
        let processor = EmbeddedPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();
        
        let php_code = r#"<html><body><?php echo "Hello from simple PHP!"; ?></body></html>"#;
        
        let result = processor.execute_php(php_code, &get_params, &post_params, &server_vars).unwrap();
        assert!(result.contains("Hello from simple PHP!"));
    }
    
    #[test]
    fn test_php_functions() {
        let processor = EmbeddedPhpProcessor::new().unwrap();
        let get_params = HashMap::new();
        let post_params = HashMap::new();
        let server_vars = HashMap::new();
        
        let php_code = r#"<?php echo phpversion(); ?>"#;
        
        let result = processor.execute_php(php_code, &get_params, &post_params, &server_vars).unwrap();
        assert!(result.contains("8.4.0-simple"));
    }
}