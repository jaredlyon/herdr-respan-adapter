use crate::Result;
use toml_edit::{DocumentMut, Item, Table, Value};

pub fn encode(item: &Item) -> String { let mut doc = DocumentMut::new(); doc["value"] = item.clone(); doc.to_string() }
pub fn decode(text: &str) -> Result<Item> { let doc: DocumentMut = text.parse()?; Ok(doc.get("value").ok_or("invalid owned TOML value")?.clone()) }
fn same(a: &Item, b: &Item) -> bool { toml::from_str::<toml::Value>(&encode(a)).ok() == toml::from_str::<toml::Value>(&encode(b)).ok() }

// Array elements retain parser decoration; compare their values without copying or formatting.
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::String(a), Value::String(b)) => a.value() == b.value(),
        (Value::Integer(a), Value::Integer(b)) => a.value() == b.value(),
        (Value::Float(a), Value::Float(b)) => a.value() == b.value(),
        (Value::Boolean(a), Value::Boolean(b)) => a.value() == b.value(),
        (Value::Datetime(a), Value::Datetime(b)) => a.value() == b.value(),
        (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b.iter()).all(|(a, b)| same_value(a, b)),
        (Value::InlineTable(a), Value::InlineTable(b)) => a.len() == b.len() && a.iter().all(|(key, value)| b.get(key).is_some_and(|other| same_value(value, other))),
        _ => false,
    }
}
fn parent<'a>(doc: &'a mut DocumentMut, path: &[String], create: bool) -> Result<(&'a mut dyn toml_edit::TableLike, String, Vec<Vec<String>>)> {
    let (last, prefix) = path.split_last().ok_or("empty TOML path")?;
    let mut table: &mut dyn toml_edit::TableLike = doc.as_table_mut(); let mut created = Vec::new();
    for (i, key) in prefix.iter().enumerate() {
        if !table.contains_key(key) { if !create { return Err("missing TOML path".into()); } table.insert(key, Item::Table(Table::new())); created.push(prefix[..=i].to_vec()); }
        table = table.get_mut(key).and_then(Item::as_table_like_mut).ok_or("TOML path traverses a non-table")?;
    }
    Ok((table, last.clone(), created))
}
pub fn insert(doc: &mut DocumentMut, path: &[String], value: &Item) -> Result<(bool, bool, Vec<Vec<String>>)> {
    insert_inner(doc, path, value, false)
}
/// Ownership is already recorded, so restore any missing members of our insertion.
pub(super) fn restore(doc: &mut DocumentMut, path: &[String], value: &Item) -> Result<(bool, bool, Vec<Vec<String>>)> {
    insert_inner(doc, path, value, true)
}
fn insert_inner(doc: &mut DocumentMut, path: &[String], value: &Item, owned: bool) -> Result<(bool, bool, Vec<Vec<String>>)> {
    let (table, key, mut created) = parent(doc, path, true)?;
    if let Some(existing) = table.get_mut(&key) {
        if same(existing, value) { return Ok((false, false, created)); }
        if let (Some(dest), Some(source)) = (existing.as_array_of_tables_mut(), value.as_array_of_tables()) {
            let overlap = source.iter().filter(|item| dest.iter().any(|t| same(&Item::Table(t.clone()), &Item::Table((*item).clone())))).count();
            if overlap == source.len() { return Ok((false, true, created)); }
            if overlap > 0 && !owned { return Err("partially overlapping TOML insertion; split into individual edits".into()); }
            let mut added = false;
            for item in source.iter() {
                if !dest.iter().any(|t| same(&Item::Table(t.clone()), &Item::Table(item.clone()))) { dest.push(item.clone()); added = true; }
            }
            return Ok((added, true, created));
        }
        if let (Some(dest), Some(source)) = (existing.as_array_mut(), value.as_array()) {
            let overlap = source.iter().filter(|item| dest.iter().any(|v| same_value(v, item))).count();
            if overlap == source.len() { return Ok((false, true, created)); }
            if overlap > 0 && !owned { return Err("partially overlapping TOML insertion; split into individual edits".into()); }
            let mut added = false;
            for item in source.iter() { if !dest.iter().any(|v| same_value(v, item)) { dest.push(item.clone()); added = true; } }
            return Ok((added, true, created));
        }
        return Err("refusing to replace foreign TOML value".into());
    }
    table.insert(&key, value.clone()); created.push(path.to_vec()); Ok((true, value.is_array_of_tables() || value.is_array(), created))
}
pub fn remove(doc: &mut DocumentMut, path: &[String], value: &Item, appended: bool, created: &[Vec<String>]) -> Result<()> {
    if let Ok((table, key, _)) = parent(doc, path, false) {
        if let Some(existing) = table.get_mut(&key) {
            if !appended { if same(existing, value) { table.remove(&key); } }
            else if let (Some(dest), Some(source)) = (existing.as_array_of_tables_mut(), value.as_array_of_tables()) {
                for item in source.iter() { if let Some(i) = dest.iter().enumerate().filter(|(_,t)| same(&Item::Table((*t).clone()), &Item::Table(item.clone()))).map(|(i,_)| i).last() { dest.remove(i); } }
            } else if let (Some(dest), Some(source)) = (existing.as_array_mut(), value.as_array()) {
                for item in source.iter() { if let Some(i) = dest.iter().enumerate().filter(|(_,v)| same_value(v, item)).map(|(i,_)| i).last() { dest.remove(i); } }
            }
        }
    }
    let mut created = created.to_vec(); created.sort_by_key(|p| std::cmp::Reverse(p.len()));
    for path in created {
        if let Ok((table, key, _)) = parent(doc, &path, false) {
            let empty = table.get(&key).is_some_and(|i| i.as_table_like().is_some_and(|t| t.is_empty()) || i.as_array().is_some_and(|a| a.is_empty()) || i.as_array_of_tables().is_some_and(|a| a.is_empty()));
            if empty { table.remove(&key); }
        }
    }
    Ok(())
}

#[cfg(test)] mod tests {
    use super::*;
    #[test] fn inserts_and_removes_owned_scalar_in_inline_table() {
        let original = "features = { other = true }\n";
        let mut doc: DocumentMut = original.parse().unwrap();
        let path = vec!["features".into(), "hooks".into()];
        let value = toml_edit::value(true);
        let (added, appended, created) = insert(&mut doc, &path, &value).unwrap();
        assert!(added);
        assert!(!insert(&mut doc, &path, &value).unwrap().0);
        assert!(insert(&mut doc, &path, &toml_edit::value(false)).is_err());
        remove(&mut doc, &path, &value, appended, &created).unwrap();
        assert_eq!(doc.to_string(), original);
    }
}
