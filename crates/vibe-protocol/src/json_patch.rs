//! JSON Patch per RFC 6902/6901 plus the wire's `append` op
//! (string-suffix streaming; `_patch.py` lowers it to a `replace`).

use serde_json::Value;

use crate::models::JsonPatchOperation;

#[derive(Debug, thiserror::Error)]
pub enum PatchError {
    #[error("invalid pointer: {0}")]
    InvalidPointer(String),
    #[error("path not found: {0}")]
    NotFound(String),
    #[error("test failed at {0}")]
    TestFailed(String),
    #[error("append requires string values at {0}")]
    AppendNotString(String),
    #[error("invalid index in path {0}")]
    InvalidIndex(String),
}

fn unescape(segment: &str) -> String {
    segment.replace("~1", "/").replace("~0", "~")
}

fn segments(path: &str) -> Result<Vec<String>, PatchError> {
    if path.is_empty() {
        return Ok(vec![]);
    }
    if !path.starts_with('/') {
        return Err(PatchError::InvalidPointer(path.to_string()));
    }
    Ok(path[1..].split('/').map(unescape).collect())
}

fn resolve<'a>(doc: &'a Value, segs: &[String]) -> Result<&'a Value, PatchError> {
    let mut cur = doc;
    for seg in segs {
        cur = match cur {
            Value::Object(map) => map
                .get(seg.as_str())
                .ok_or_else(|| PatchError::NotFound(seg.clone()))?,
            Value::Array(arr) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| PatchError::InvalidIndex(seg.clone()))?;
                arr.get(idx)
                    .ok_or_else(|| PatchError::NotFound(seg.clone()))?
            }
            _ => return Err(PatchError::NotFound(seg.clone())),
        };
    }
    Ok(cur)
}

fn resolve_mut<'a>(doc: &'a mut Value, segs: &[String]) -> Result<&'a mut Value, PatchError> {
    let mut cur = doc;
    for seg in segs {
        cur = match cur {
            Value::Object(map) => map
                .get_mut(seg.as_str())
                .ok_or_else(|| PatchError::NotFound(seg.clone()))?,
            Value::Array(arr) => {
                let idx: usize = seg
                    .parse()
                    .map_err(|_| PatchError::InvalidIndex(seg.clone()))?;
                arr.get_mut(idx)
                    .ok_or_else(|| PatchError::NotFound(seg.clone()))?
            }
            _ => return Err(PatchError::NotFound(seg.clone())),
        };
    }
    Ok(cur)
}

fn add(doc: &mut Value, segs: &[String], value: Value) -> Result<(), PatchError> {
    if segs.is_empty() {
        *doc = value;
        return Ok(());
    }
    let (parent_segs, last) = segs.split_at(segs.len() - 1);
    let key = &last[0];
    let parent = resolve_mut(doc, parent_segs)?;
    match parent {
        Value::Object(map) => {
            map.insert(key.clone(), value);
            Ok(())
        }
        Value::Array(arr) => {
            if key == "-" {
                arr.push(value);
                return Ok(());
            }
            let idx: usize = key
                .parse()
                .map_err(|_| PatchError::InvalidIndex(key.clone()))?;
            if idx > arr.len() {
                return Err(PatchError::InvalidIndex(key.clone()));
            }
            arr.insert(idx, value);
            Ok(())
        }
        _ => Err(PatchError::NotFound(key.clone())),
    }
}

fn remove(doc: &mut Value, segs: &[String]) -> Result<Value, PatchError> {
    if segs.is_empty() {
        return Err(PatchError::InvalidPointer("cannot remove root".into()));
    }
    let (parent_segs, last) = segs.split_at(segs.len() - 1);
    let key = &last[0];
    let parent = resolve_mut(doc, parent_segs)?;
    match parent {
        Value::Object(map) => map
            .remove(key.as_str())
            .ok_or_else(|| PatchError::NotFound(key.clone())),
        Value::Array(arr) => {
            let idx: usize = key
                .parse()
                .map_err(|_| PatchError::InvalidIndex(key.clone()))?;
            if idx >= arr.len() {
                return Err(PatchError::NotFound(key.clone()));
            }
            Ok(arr.remove(idx))
        }
        _ => Err(PatchError::NotFound(key.clone())),
    }
}

/// Apply one operation in place.
pub fn apply_operation(doc: &mut Value, op: &JsonPatchOperation) -> Result<(), PatchError> {
    let segs = segments(&op.path)?;
    match op.op.as_str() {
        "add" => add(doc, &segs, op.value.clone()),
        "replace" => {
            if segs.is_empty() {
                *doc = op.value.clone();
                return Ok(());
            }
            let slot = resolve_mut(doc, &segs)?;
            *slot = op.value.clone();
            Ok(())
        }
        "remove" => remove(doc, &segs).map(|_| ()),
        "test" => {
            let current = resolve(doc, &segs)?;
            if *current == op.value {
                Ok(())
            } else {
                Err(PatchError::TestFailed(op.path.clone()))
            }
        }
        "append" => {
            let slot = resolve_mut(doc, &segs)?;
            match (slot, &op.value) {
                (Value::String(current), Value::String(suffix)) => {
                    current.push_str(suffix);
                    Ok(())
                }
                _ => Err(PatchError::AppendNotString(op.path.clone())),
            }
        }
        other => Err(PatchError::InvalidPointer(format!(
            "unsupported op {other}"
        ))),
    }
}

/// Apply a patch list to a document, returning the patched clone.
pub fn apply_patch(doc: &Value, ops: &[JsonPatchOperation]) -> Result<Value, PatchError> {
    let mut out = doc.clone();
    for op in ops {
        apply_operation(&mut out, op)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(o: &str, path: &str, value: Value) -> JsonPatchOperation {
        JsonPatchOperation {
            op: o.into(),
            path: path.into(),
            value,
        }
    }

    #[test]
    fn replace_and_append() {
        let mut doc = json!({"state": {"outputText": "hello"}});
        apply_operation(
            &mut doc,
            &op("append", "/state/outputText", json!(" world")),
        )
        .unwrap();
        assert_eq!(doc["state"]["outputText"], "hello world");
    }

    #[test]
    fn add_remove_array() {
        let mut doc = json!({"items": [1, 2]});
        apply_operation(&mut doc, &op("add", "/items/1", json!(9))).unwrap();
        assert_eq!(doc["items"], json!([1, 9, 2]));
        apply_operation(&mut doc, &op("add", "/items/-", json!(3))).unwrap();
        assert_eq!(doc["items"], json!([1, 9, 2, 3]));
        apply_operation(&mut doc, &op("remove", "/items/0", Value::Null)).unwrap();
        assert_eq!(doc["items"], json!([9, 2, 3]));
    }

    #[test]
    fn escaped_segments() {
        let mut doc = json!({"a/b": {"~x": 1}});
        apply_operation(&mut doc, &op("replace", "/a~1b/~0x", json!(2))).unwrap();
        assert_eq!(doc["a/b"]["~x"], 2);
    }

    #[test]
    fn test_op() {
        let mut doc = json!({"n": 5});
        apply_operation(&mut doc, &op("test", "/n", json!(5))).unwrap();
        assert!(matches!(
            apply_operation(&mut doc, &op("test", "/n", json!(6))),
            Err(PatchError::TestFailed(_))
        ));
    }
}
