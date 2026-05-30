//! XML parser module for SimpleXML-compatible functionality
use anyhow::{anyhow, Result};
use roxmltree::{Document, Node};
use std::collections::HashMap;
use crate::ast_php_processor::{PhpValue, PhpArrayItem, PhpObject};

/// Convert an XML string to a PHP-compatible SimpleXMLElement object
pub fn parse_xml_to_php_value(xml_string: &str) -> Result<PhpValue> {
    let doc = Document::parse(xml_string)
        .map_err(|e| anyhow!("XML parsing error: {}", e))?;

    let root = doc.root_element();
    Ok(node_to_php_value(&root))
}

/// Convert an XML node to a PHP value recursively
fn node_to_php_value(node: &Node) -> PhpValue {
    let mut props = HashMap::new();
    let mut children_by_name: HashMap<String, Vec<PhpValue>> = HashMap::new();
    let mut attributes = HashMap::new();

    // Collect attributes
    for attr in node.attributes() {
        attributes.insert(
            format!("@{}", attr.name()),
            PhpValue::String(attr.value().to_string())
        );
    }

    // If node has attributes, add them as @attributes property
    if !attributes.is_empty() {
        let attr_items: Vec<PhpArrayItem> = attributes
            .into_iter()
            .map(|(k, v)| PhpArrayItem::KeyValue(k, v))
            .collect();
        props.insert("@attributes".to_string(), PhpValue::Array(attr_items));
    }

    // Process child nodes
    let mut text_content = String::new();
    let mut has_element_children = false;

    for child in node.children() {
        if child.is_element() {
            has_element_children = true;
            let child_name = child.tag_name().name().to_string();
            let child_value = node_to_php_value(&child);

            children_by_name
                .entry(child_name)
                .or_insert_with(Vec::new)
                .push(child_value);
        } else if child.is_text() {
            if let Some(text) = child.text() {
                text_content.push_str(text);
            }
        }
    }

    // Add children to properties
    for (name, mut values) in children_by_name {
        let value = if values.len() == 1 {
            values.pop().unwrap()
        } else {
            // Multiple children with same name become an array
            let items: Vec<PhpArrayItem> = values
                .into_iter()
                .map(PhpArrayItem::Value)
                .collect();
            PhpValue::Array(items)
        };
        props.insert(name, value);
    }

    // Handle text content
    let trimmed_text = text_content.trim();
    if !trimmed_text.is_empty() {
        if has_element_children {
            // Mixed content: both text and elements
            props.insert("_text".to_string(), PhpValue::String(trimmed_text.to_string()));
        } else if props.is_empty() {
            // Only text content, no attributes or children
            return PhpValue::String(trimmed_text.to_string());
        } else {
            // Has attributes but only text content
            props.insert("_value".to_string(), PhpValue::String(trimmed_text.to_string()));
        }
    }

    // Create SimpleXMLElement object
    PhpValue::Object(Box::new(PhpObject {
        class_name: "SimpleXMLElement".to_string(),
        props,
    }))
}

/// Helper to extract text content from a SimpleXMLElement
pub fn get_xml_text(value: &PhpValue) -> String {
    match value {
        PhpValue::String(s) => s.clone(),
        PhpValue::Object(obj) if obj.class_name == "SimpleXMLElement" => {
            // Check for _value first (text with attributes)
            if let Some(PhpValue::String(s)) = obj.props.get("_value") {
                return s.clone();
            }
            // Check for _text (mixed content)
            if let Some(PhpValue::String(s)) = obj.props.get("_text") {
                return s.clone();
            }
            // Otherwise, concatenate all text from child elements
            let mut text = String::new();
            for (key, val) in &obj.props {
                if !key.starts_with('@') && key != "_text" && key != "_value" {
                    text.push_str(&get_xml_text(val));
                }
            }
            text
        }
        PhpValue::Array(items) => {
            let mut text = String::new();
            for item in items {
                match item {
                    PhpArrayItem::Value(v) | PhpArrayItem::KeyValue(_, v) => {
                        text.push_str(&get_xml_text(v));
                    }
                }
            }
            text
        }
        _ => String::new(),
    }
}

/// Helper to get child element by name (supports chained access like $xml->child->subchild)
pub fn get_xml_child(value: &PhpValue, child_name: &str) -> PhpValue {
    match value {
        PhpValue::Object(obj) if obj.class_name == "SimpleXMLElement" => {
            obj.props.get(child_name)
                .cloned()
                .unwrap_or(PhpValue::Null)
        }
        PhpValue::Array(items) => {
            // If it's an array, return the first element when accessed as property
            if let Some(PhpArrayItem::Value(v)) = items.first() {
                get_xml_child(v, child_name)
            } else {
                PhpValue::Null
            }
        }
        _ => PhpValue::Null,
    }
}

/// Helper to get XML attributes
#[allow(dead_code)]
pub fn get_xml_attributes(value: &PhpValue) -> HashMap<String, String> {
    let mut attrs = HashMap::new();

    if let PhpValue::Object(obj) = value {
        if obj.class_name == "SimpleXMLElement" {
            if let Some(PhpValue::Array(attr_items)) = obj.props.get("@attributes") {
                for item in attr_items {
                    if let PhpArrayItem::KeyValue(key, PhpValue::String(val)) = item {
                        // Remove @ prefix if present
                        let attr_key = key.strip_prefix('@').unwrap_or(key);
                        attrs.insert(attr_key.to_string(), val.clone());
                    }
                }
            }
        }
    }

    attrs
}

/// Convert SimpleXMLElement to array (like PHP's json_decode(json_encode($xml), true))
#[allow(dead_code)]
pub fn xml_to_array(value: &PhpValue) -> PhpValue {
    match value {
        PhpValue::Object(obj) if obj.class_name == "SimpleXMLElement" => {
            let mut items = Vec::new();

            for (key, val) in &obj.props {
                if key == "@attributes" {
                    // Keep attributes as-is
                    items.push(PhpArrayItem::KeyValue(key.clone(), val.clone()));
                } else if key == "_value" || key == "_text" {
                    // Text content becomes direct value
                    if let PhpValue::String(s) = val {
                        return PhpValue::String(s.clone());
                    }
                } else {
                    // Recursively convert child elements
                    items.push(PhpArrayItem::KeyValue(key.clone(), xml_to_array(val)));
                }
            }

            if items.is_empty() {
                PhpValue::Null
            } else {
                PhpValue::Array(items)
            }
        }
        PhpValue::Array(arr_items) => {
            let converted: Vec<PhpArrayItem> = arr_items.iter().map(|item| {
                match item {
                    PhpArrayItem::Value(v) => PhpArrayItem::Value(xml_to_array(v)),
                    PhpArrayItem::KeyValue(k, v) => PhpArrayItem::KeyValue(k.clone(), xml_to_array(v)),
                }
            }).collect();
            PhpValue::Array(converted)
        }
        _ => value.clone(),
    }
}