use crate::Result;
use serde_json::{Map, Value};

fn parts(pointer: &str) -> Result<Vec<String>> {
    if pointer.is_empty() { return Ok(Vec::new()); }
    if !pointer.starts_with('/') { return Err("JSON pointer must start with /".into()); }
    Ok(pointer[1..].split('/').map(|s| s.replace("~1", "/").replace("~0", "~")).collect())
}
fn escape(key: &str) -> String { key.replace('~', "~0").replace('/', "~1") }
pub fn insert(root: &mut Value, pointer: &str, value: &Value) -> Result<(bool, Vec<String>)> {
    let parts = parts(pointer)?; let mut node = root; let mut prefix = String::new(); let mut created = Vec::new();
    for (i, key) in parts.iter().enumerate() {
        prefix.push('/'); prefix.push_str(&escape(key));
        if let Value::Object(map) = node {
            if !map.contains_key(key) { map.insert(key.clone(), if i+1 == parts.len() { Value::Array(Vec::new()) } else { Value::Object(Map::new()) }); created.push(prefix.clone()); }
            node = map.get_mut(key).ok_or("missing JSON key")?;
        } else if let Value::Array(array) = node { node = array.get_mut(key.parse::<usize>()?).ok_or("invalid JSON array index")?; }
        else { return Err("JSON pointer traverses a scalar".into()); }
    }
    let array = node.as_array_mut().ok_or("JSON hook destination is not an array")?;
    if array.contains(value) { return Ok((false, created)); }
    array.push(value.clone()); Ok((true, created))
}
pub fn remove(root: &mut Value, pointer: &str, value: &Value, created: &[String]) -> Result<bool> {
    let mut removed = false;
    if let Some(array) = root.pointer_mut(pointer).and_then(Value::as_array_mut) {
        if let Some(index) = array.iter().rposition(|v| v == value) { array.remove(index); removed = true; }
    }
    let mut created = created.to_vec(); created.sort_by_key(|s| std::cmp::Reverse(s.len()));
    for pointer in created {
        let Some((parent, key)) = pointer.rsplit_once('/') else { continue; };
        if root.pointer(&pointer).is_some_and(|v| v.as_object().is_some_and(Map::is_empty) || v.as_array().is_some_and(Vec::is_empty)) {
            if let Some(map) = root.pointer_mut(parent).and_then(Value::as_object_mut) { map.remove(&key.replace("~1", "/").replace("~0", "~")); }
        }
    }
    Ok(removed)
}
