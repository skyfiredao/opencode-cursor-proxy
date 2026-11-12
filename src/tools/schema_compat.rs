use super::alias::{family, key};
use crate::error::ApiError;
use serde_json::{Map, Value, json};

fn argument_family(name: &str) -> String {
    let k = key(name);
    match k.as_str() {
        "filepath" | "filename" | "file" | "targetpath" | "directorypath" | "dir" | "folder"
        | "directory" | "targetdirectory" | "targetfile" => "path".to_owned(),
        "globpattern" | "filepattern" | "searchpattern" => "pattern".to_owned(),
        "includepattern" => "include".to_owned(),
        "workingdirectory" | "workdir" | "currentdirectory" => "cwd".to_owned(),
        "cmd" | "script" | "shellcommand" | "terminalcommand" => "command".to_owned(),
        "contents" | "text" | "body" | "data" | "payload" | "streamcontent" => "content".to_owned(),
        "subagenttype" | "agent" => "agent".to_owned(),
        _ => k,
    }
}

pub fn full_file_write(args: &Value) -> Option<Value> {
    let map = args.as_object()?;
    if map.keys().any(|k| key(k) == "oldstring") {
        return None;
    }
    let path = map
        .iter()
        .find(|(k, _)| argument_family(k) == "path")?
        .1
        .as_str()?;
    let content = map
        .iter()
        .find(|(k, _)| argument_family(k) == "content")?
        .1
        .as_str()?;
    Some(json!({"path":path, "content":content}))
}

pub fn normalize(tool: &str, args: Value, schema: &Value) -> Result<Value, ApiError> {
    let Value::Object(args) = args else {
        return Err(ApiError::upstream(format!(
            "Arguments for {tool} must be an object"
        )));
    };
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Ok(Value::Object(args));
    };
    let mut normalized = Map::new();
    for (name, value) in args {
        let target = if properties.contains_key(&name) {
            name.clone()
        } else {
            let matches: Vec<_> = properties
                .keys()
                .filter(|p| argument_family(p) == argument_family(&name))
                .collect();
            if matches.len() == 1 {
                matches[0].to_owned()
            } else {
                name.clone()
            }
        };
        if let Some(existing) = normalized.get(&target) {
            if existing != &value {
                return Err(ApiError::upstream(format!(
                    "Conflicting argument aliases for {tool}.{target}"
                )));
            }
        } else {
            normalized.insert(target, value);
        }
    }
    if family(tool) == "task" {
        for field in ["agent", "subagent_type"] {
            if let Some(value) = normalized.get_mut(field) {
                if let Some(custom) = value.get("custom").and_then(Value::as_str) {
                    *value = Value::String(custom.to_owned());
                }
                if value
                    .as_str()
                    .is_some_and(|s| matches!(key(s).as_str(), "unspecified" | "generalpurpose"))
                {
                    *value = Value::String("general".to_owned());
                }
            }
        }
        if tool == "subagent"
            && normalized
                .get("model")
                .and_then(Value::as_str)
                .is_some_and(|s| !s.contains('/'))
        {
            normalized.remove("model");
        }
    }
    if family(tool) == "question" {
        let title = normalized
            .remove("title")
            .and_then(|v| v.as_str().map(str::to_owned));
        if let Some(questions) = normalized
            .get_mut("questions")
            .and_then(Value::as_array_mut)
        {
            for question in questions {
                if let Some(question) = question.as_object_mut() {
                    for alias in ["prompt", "text"] {
                        if let Some(value) = question.remove(alias) {
                            question.entry("question").or_insert(value);
                        }
                    }
                    for field in ["allowMultiple", "allow_multiple"] {
                        if let Some(value) = question.remove(field) {
                            question.entry("multiple").or_insert(value);
                        }
                    }
                    let header = question
                        .get("header")
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .or(title.as_deref())
                        .or_else(|| question.get("question").and_then(Value::as_str))
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(30)
                        .collect::<String>();
                    question.insert("header".to_owned(), Value::String(header));
                    question.remove("id");
                    if let Some(header) = question.get_mut("header")
                        && let Some(text) = header.as_str()
                    {
                        *header = Value::String(text.chars().take(30).collect());
                    }
                    if let Some(options) = question.get_mut("options").and_then(Value::as_array_mut)
                    {
                        for option in options {
                            if let Some(text) = option.as_str() {
                                *option = json!({"label":text.trim().chars().take(30).collect::<String>(),"description":text});
                            } else if let Some(option) = option.as_object_mut() {
                                let label = ["label", "title", "text", "description"]
                                    .iter()
                                    .find_map(|k| {
                                        option
                                            .get(*k)
                                            .and_then(Value::as_str)
                                            .filter(|s| !s.trim().is_empty())
                                    })
                                    .unwrap_or("")
                                    .to_owned();
                                if option
                                    .get("description")
                                    .and_then(Value::as_str)
                                    .is_none_or(|s| s.trim().is_empty())
                                {
                                    option.insert(
                                        "description".to_owned(),
                                        Value::String(label.clone()),
                                    );
                                }
                                option.insert(
                                    "label".to_owned(),
                                    Value::String(label.trim().chars().take(30).collect()),
                                );
                                for field in ["id", "title", "text"] {
                                    option.remove(field);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(Value::Object(normalized))
}
