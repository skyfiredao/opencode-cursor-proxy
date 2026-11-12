use serde_json::{Map, Value};
use std::{
    fs::{self, OpenOptions},
    io::{ErrorKind, Read, Write},
    ops::Range,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const LIMIT: u64 = 8 * 1024 * 1024;
static PUBLICATION: Mutex<()> = Mutex::new(());

pub fn publish_context_limit(
    path: &Path,
    model: &str,
    capacity: u64,
    cancelled: &tokio_util::sync::CancellationToken,
) -> Result<bool, Box<dyn std::error::Error>> {
    publish_context_limit_for(
        crate::opencode::Version::V2,
        path,
        model,
        capacity,
        cancelled,
    )
}

pub fn publish_context_limit_for(
    version: crate::opencode::Version,
    path: &Path,
    model: &str,
    capacity: u64,
    cancelled: &tokio_util::sync::CancellationToken,
) -> Result<bool, Box<dyn std::error::Error>> {
    let _publication = PUBLICATION
        .lock()
        .map_err(|_| "Configuration publication lock poisoned")?;
    if cancelled.is_cancelled() || capacity == 0 {
        return Ok(false);
    }
    let path = match path.canonicalize() {
        Ok(path) => path,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if !fs::metadata(&path)?.is_file() {
        return Err("OpenCode configuration must be a regular file".into());
    }
    let mut previous = Vec::new();
    fs::File::open(&path)?
        .take(LIMIT + 1)
        .read_to_end(&mut previous)?;
    if previous.len() as u64 > LIMIT {
        return Err("OpenCode configuration exceeds 8 MiB".into());
    }
    let source = std::str::from_utf8(&previous)?;
    let parsed = parse(source)?;
    let definition = parsed[version.providers()]["cursor-proxy"]
        .get("models")
        .and_then(Value::as_object)
        .and_then(|models| models.get(model));
    let Some(definition) = definition else {
        return Ok(false);
    };
    let context = definition.pointer("/limit/context");
    let unknown_v1 =
        version == crate::opencode::Version::V1 && context.and_then(Value::as_u64) == Some(0);
    if !definition.is_object() || (context.is_some() && !unknown_v1) {
        return Ok(false);
    }
    let mut limit = serde_json::json!({"context":capacity});
    if version == crate::opencode::Version::V1 && definition.pointer("/limit/output").is_none() {
        limit["output"] = serde_json::json!(0);
    }
    let output = merge(
        source,
        &serde_json::json!({(version.providers()):{"cursor-proxy":{"models":{(model):{"limit":limit}}}}}),
    )?;
    Ok(persist(path, Some(previous), output, Some(cancelled))?.changed)
}

fn clean(source: &str) -> Result<Vec<u8>, String> {
    let mut bytes = source.as_bytes().to_vec();
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        bytes[..3].fill(b' ');
    }
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => index = string_end(&bytes, index)?,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                while index < bytes.len() && bytes[index] != b'\n' {
                    bytes[index] = b' ';
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                bytes[index..index + 2].fill(b' ');
                index += 2;
                loop {
                    if index + 1 >= bytes.len() {
                        return Err("Unterminated JSONC comment".to_owned());
                    }
                    if bytes[index..index + 2] == *b"*/" {
                        bytes[index..index + 2].fill(b' ');
                        index += 2;
                        break;
                    }
                    if bytes[index] != b'\n' && bytes[index] != b'\r' {
                        bytes[index] = b' ';
                    }
                    index += 1;
                }
            }
            _ => index += 1,
        }
    }
    Ok(bytes)
}

fn string_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Ok(index + 1),
            _ => index += 1,
        }
    }
    Err("Unterminated JSON string".to_owned())
}

pub fn parse(source: &str) -> Result<Value, String> {
    let mut bytes = clean(source)?;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            index = string_end(&bytes, index)?;
            continue;
        }
        if bytes[index] == b',' {
            let next = skip_space(&bytes, index + 1);
            let previous = bytes[..index]
                .iter()
                .rfind(|byte| !byte.is_ascii_whitespace());
            if matches!(bytes.get(next), Some(b'}' | b']'))
                && !matches!(previous, None | Some(b'{' | b'[' | b':' | b','))
            {
                bytes[index] = b' ';
            }
        }
        index += 1;
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("Invalid OpenCode JSONC: {error}"))
}

fn skip_space(bytes: &[u8], mut index: usize) -> usize {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    index
}

fn value_end(bytes: &[u8], start: usize) -> Result<usize, String> {
    if bytes.get(start) == Some(&b'"') {
        return string_end(bytes, start);
    }
    let mut depth = 0;
    let mut index = start;
    while index < bytes.len() {
        match bytes[index] {
            b'"' => {
                index = string_end(bytes, index)?;
                continue;
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' if depth == 0 => break,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(index + 1);
                }
            }
            b',' if depth == 0 => break,
            byte if depth == 0 && byte.is_ascii_whitespace() => break,
            _ => (),
        }
        index += 1;
    }
    Ok(index)
}

fn fields(
    bytes: &[u8],
) -> Result<(Map<String, Value>, Vec<(String, Range<usize>)>, usize), String> {
    let source = std::str::from_utf8(bytes).map_err(|error| error.to_string())?;
    let value = parse(source)?;
    let object = value
        .as_object()
        .ok_or("Configuration node must be an object")?
        .clone();
    let start = skip_space(bytes, 0);
    let mut index = skip_space(bytes, start + 1);
    let mut result: Vec<(String, Range<usize>)> = Vec::new();
    while bytes.get(index) != Some(&b'}') {
        let end = string_end(bytes, index)?;
        let key: String =
            serde_json::from_slice(&bytes[index..end]).map_err(|error| error.to_string())?;
        if result.iter().any(|(name, _)| name == &key) {
            return Err(format!("Duplicate configuration key: {key}"));
        }
        index = skip_space(bytes, end);
        if bytes.get(index) != Some(&b':') {
            return Err("Missing configuration key separator".to_owned());
        }
        let begin = skip_space(bytes, index + 1);
        let end = value_end(bytes, begin)?;
        result.push((key, begin..end));
        index = skip_space(bytes, end);
        if bytes.get(index) == Some(&b',') {
            index = skip_space(bytes, index + 1);
        }
    }
    Ok((object, result, index))
}

pub fn merge(source: &str, patch: &Value) -> Result<String, String> {
    let patch = patch
        .as_object()
        .ok_or("Configuration patch must be an object")?;
    let mut output = source.to_owned();
    for (key, value) in patch {
        let bytes = clean(&output)?;
        let (object, members, closing) = fields(&bytes)?;
        if let Some((_, span)) = members.iter().find(|(name, _)| name == key) {
            let current = &object[key];
            let replacement = if value.is_object() {
                if !current.is_object() {
                    return Err(format!("Configuration key {key} must be an object"));
                }
                merge(&output[span.clone()], value)?
            } else if current == value {
                continue;
            } else {
                serde_json::to_string(value).map_err(|error| error.to_string())?
            };
            output.replace_range(span.clone(), &replacement);
        } else {
            let member = format!(
                "\n  {}: {}\n",
                serde_json::to_string(key).map_err(|error| error.to_string())?,
                serde_json::to_string(value).map_err(|error| error.to_string())?
            );
            output.insert_str(closing, &member);
            if let Some((_, last)) = members.last()
                && bytes.get(skip_space(&bytes, last.end)) != Some(&b',')
            {
                output.insert(last.end, ',');
            }
        }
    }
    parse(&output)?;
    Ok(output)
}

fn remove_key(source: &str, key: &str) -> Result<String, String> {
    let bytes = clean(source)?;
    let (_, members, _) = fields(&bytes)?;
    let Some(index) = members.iter().position(|(name, _)| name == key) else {
        return Ok(source.to_owned());
    };
    let start = if index == 0 {
        skip_space(&bytes, skip_space(&bytes, 0) + 1)
    } else {
        skip_space(&bytes, skip_space(&bytes, members[index - 1].1.end) + 1)
    };
    let end = members[index].1.end;
    let next = skip_space(&bytes, end);
    let mut output = source.to_owned();
    if bytes.get(next) == Some(&b',') {
        output.replace_range(start..next + 1, "");
    } else {
        output.replace_range(start..end, "");
        if index > 0 {
            output.remove(skip_space(&bytes, members[index - 1].1.end));
        }
    }
    parse(&output)?;
    Ok(output)
}

fn model_reference(value: &Value) -> Option<Value> {
    if let Some(model) = value
        .as_str()
        .and_then(|model| model.strip_prefix("lmstudio/"))
    {
        return Some(Value::String(format!("cursor-proxy/{model}")));
    }
    if value.get("providerID").and_then(Value::as_str) == Some("lmstudio") {
        let mut model = value.clone();
        model["providerID"] = Value::String("cursor-proxy".to_owned());
        return Some(model);
    }
    None
}

pub fn default_path() -> Result<PathBuf, String> {
    if let Some(directory) = std::env::var_os("OPENCODE_CONFIG_DIR") {
        if directory.is_empty() {
            return Err("OPENCODE_CONFIG_DIR must not be empty".to_owned());
        }
        return Ok(configuration_file(&PathBuf::from(directory)));
    }
    let root = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            #[cfg(windows)]
            let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
            #[cfg(not(windows))]
            let home = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE"));
            home.filter(|home| !home.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .ok_or("Cannot find the OpenCode user configuration directory")?
        .join("opencode");
    Ok(configuration_file(&root))
}

fn configuration_file(directory: &Path) -> PathBuf {
    let json = directory.join("opencode.json");
    let jsonc = directory.join("opencode.jsonc");
    if jsonc.is_file() || !json.is_file() {
        jsonc
    } else {
        json
    }
}

pub struct Configured {
    pub path: PathBuf,
    pub backup: Option<PathBuf>,
    pub changed: bool,
}

fn create_file(path: &Path) -> Result<fs::File, std::io::Error> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn configure(
    path: &Path,
    base_url: Option<&str>,
    token: Option<&str>,
    models: &[crate::models::Model],
) -> Result<Configured, Box<dyn std::error::Error>> {
    configure_for(crate::opencode::Version::V2, path, base_url, token, models)
}

pub fn configure_for(
    version: crate::opencode::Version,
    path: &Path,
    base_url: Option<&str>,
    token: Option<&str>,
    models: &[crate::models::Model],
) -> Result<Configured, Box<dyn std::error::Error>> {
    let _publication = PUBLICATION
        .lock()
        .map_err(|_| "Configuration publication lock poisoned")?;
    if models.is_empty() {
        return Err("Cannot configure cursor-proxy with an empty model catalog".into());
    }
    let path = if path.exists() {
        fs::canonicalize(path)?
    } else {
        std::path::absolute(path)?
    };
    if path.exists() && !fs::metadata(&path)?.is_file() {
        return Err("OpenCode configuration must be a regular file".into());
    }
    let previous = match fs::File::open(&path) {
        Ok(file) => {
            let mut bytes = Vec::new();
            file.take(LIMIT + 1).read_to_end(&mut bytes)?;
            if bytes.len() as u64 > LIMIT {
                return Err("OpenCode configuration exceeds 8 MiB".into());
            }
            Some(bytes)
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {
            if fs::symlink_metadata(&path).is_ok() {
                return Err("Configuration is a dangling symlink".into());
            }
            None
        }
        Err(error) => return Err(error.into()),
    };
    let source = match previous.as_deref() {
        Some(bytes) => std::str::from_utf8(bytes)?,
        None => "{}\n",
    };
    let original = parse(source)?;
    let legacy = original.pointer("/providers/lmstudio").filter(|provider| {
        version == crate::opencode::Version::V2
            && provider.get("name").and_then(Value::as_str) == Some("Cursor Proxy")
            && provider.get("package").and_then(Value::as_str)
                == Some("@opencode/ai/providers/openai-compatible")
            && provider
                .pointer("/compatibility/reasoningField")
                .and_then(Value::as_str)
                == Some("reasoning_content")
            && provider
                .pointer("/settings/baseURL")
                .and_then(Value::as_str)
                .is_some()
    });
    if let Some(legacy) = legacy {
        let generated = legacy.as_object().is_some_and(|provider| {
            provider
                .keys()
                .all(|key| ["name", "package", "compatibility", "settings"].contains(&key.as_str()))
        }) && legacy
            .get("settings")
            .and_then(Value::as_object)
            .is_some_and(|settings| {
                settings
                    .keys()
                    .all(|key| ["baseURL", "apiKey"].contains(&key.as_str()))
            })
            && legacy
                .get("compatibility")
                .and_then(Value::as_object)
                .is_some_and(|compatibility| compatibility.len() == 1);
        if !generated {
            return Err("Legacy Cursor Proxy entry under lmstudio contains custom settings; migrate those settings to cursor-proxy before retrying".into());
        }
    }
    let target = &original[version.providers()]["cursor-proxy"];
    let other = &original[version.other().providers()]["cursor-proxy"];
    let endpoint = base_url
        .or_else(|| {
            target[version.settings()]
                .get("baseURL")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            legacy
                .and_then(|provider| provider.pointer("/settings/baseURL"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            other[version.other().settings()]
                .get("baseURL")
                .and_then(Value::as_str)
        })
        .unwrap_or("http://127.0.0.1:32124/v1")
        .trim_end_matches('/');
    let uri: axum::http::Uri = endpoint.parse()?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || !uri.path().ends_with("/v1")
        || uri.query().is_some()
    {
        return Err("Proxy API URL must be an HTTP(S) URL ending in /v1 without a query".into());
    }
    if token.is_some_and(|token| {
        token.is_empty() || token.chars().any(|c| c.is_whitespace() || c.is_control())
    }) {
        return Err(
            "--password must be nonempty and contain no whitespace or control characters".into(),
        );
    }
    let other_settings = &other[version.other().settings()];
    let same_endpoint = other_settings["baseURL"]
        .as_str()
        .is_some_and(|url| url.trim_end_matches('/') == endpoint);
    let token = token
        .or_else(|| {
            target[version.settings()]
                .get("apiKey")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            legacy
                .filter(|provider| {
                    provider
                        .pointer("/settings/baseURL")
                        .and_then(Value::as_str)
                        .is_some_and(|url| url.trim_end_matches('/') == endpoint)
                })
                .and_then(|provider| provider.pointer("/settings/apiKey"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            same_endpoint
                .then(|| other_settings["apiKey"].as_str())
                .flatten()
        });
    let mut patch =
        crate::models::provider_config_for(version, models, endpoint, token, "cursor-proxy");
    {
        let definitions = patch[version.providers()]["cursor-proxy"]
            .get_mut("models")
            .and_then(Value::as_object_mut)
            .ok_or("Invalid generated model configuration")?;
        for (id, definition) in definitions {
            if let Some(source) = other.get("models").and_then(|models| models.get(id)) {
                let inherited = translate_model(source, version, same_endpoint);
                *definition = parse(&merge(&serde_json::to_string(definition)?, &inherited)?)?;
            }
            if let Some(overrides) = target.get("models").and_then(|models| models.get(id)) {
                *definition = parse(&merge(&serde_json::to_string(definition)?, overrides)?)?;
            }
            if version == crate::opencode::Version::V1 {
                if let Some(limit) = definition.get_mut("limit").and_then(Value::as_object_mut) {
                    limit.entry("context").or_insert(serde_json::json!(0));
                    limit.entry("output").or_insert(serde_json::json!(0));
                }
                if let Some(cost) = definition.get("cost")
                    && (!cost["input"].is_number() || !cost["output"].is_number())
                {
                    return Err(format!("V1 model {id} requires both input and output prices when cost is configured").into());
                }
            }
        }
    }
    if original.get("$schema").is_some() {
        patch
            .as_object_mut()
            .ok_or("Invalid generated configuration")?
            .remove("$schema");
    }
    let mut working = source.to_owned();
    if legacy.is_some() {
        let bytes = clean(source)?;
        let (_, members, _) = fields(&bytes)?;
        let (_, span) = members
            .iter()
            .find(|(name, _)| name == "providers")
            .ok_or("Missing provider configuration")?;
        working.replace_range(
            span.clone(),
            &remove_key(&source[span.clone()], "lmstudio")?,
        );
        if let Some(model) = original.get("model").and_then(model_reference) {
            patch["model"] = model;
        }
        for collection in ["agents", "commands"] {
            if let Some(entries) = original.get(collection).and_then(Value::as_object) {
                for (name, entry) in entries {
                    if let Some(model) = entry.get("model").and_then(model_reference) {
                        patch[collection][name] = serde_json::json!({"model": model});
                    }
                }
            }
        }
    }
    let output = merge(&working, &patch)?;
    if output == source {
        return Ok(Configured {
            path,
            backup: None,
            changed: false,
        });
    }
    persist(path, previous, output, None)
}

fn translate_model(source: &Value, target: crate::opencode::Version, same_endpoint: bool) -> Value {
    use crate::opencode::Version;
    let mut result = serde_json::json!({});
    for key in ["name", "family", "limit"] {
        if let Some(value) = source.get(key) {
            result[key] = value.clone();
        }
    }
    if same_endpoint && let Some(headers) = source.get("headers") {
        result["headers"] = headers.clone();
    }
    let (source_id, target_id) = if target == Version::V1 {
        ("modelID", "id")
    } else {
        ("id", "modelID")
    };
    if let Some(value) = source.get(source_id) {
        result[target_id] = value.clone();
    }
    if let Some(cost) = source.get("cost") {
        for key in ["input", "output"] {
            if let Some(value) = cost.get(key) {
                result["cost"][key] = value.clone();
            }
        }
        for (old, new) in [("cache_read", "read"), ("cache_write", "write")] {
            if target == Version::V1 {
                if let Some(value) = cost.get("cache").and_then(|cache| cache.get(new)) {
                    result["cost"][old] = value.clone();
                }
            } else if let Some(value) = cost.get(old) {
                result["cost"]["cache"][new] = value.clone();
            }
        }
    }
    for (old, new) in [("tool_call", "tools"), ("reasoning", "reasoning")] {
        if target == Version::V1 {
            if let Some(value) = source.get("capabilities").and_then(|cap| cap.get(new)) {
                result[old] = value.clone();
            }
        } else if let Some(value) = source.get(old) {
            result["capabilities"][new] = value.clone();
        }
    }
    for direction in ["input", "output"] {
        if target == Version::V1 {
            if let Some(value) = source
                .get("capabilities")
                .and_then(|cap| cap.get(direction))
            {
                result["modalities"][direction] = value.clone();
            }
        } else if let Some(value) = source.get("modalities").and_then(|cap| cap.get(direction)) {
            result["capabilities"][direction] = value.clone();
        }
    }
    result
}

fn persist(
    path: PathBuf,
    previous: Option<Vec<u8>>,
    output: String,
    cancelled: Option<&tokio_util::sync::CancellationToken>,
) -> Result<Configured, Box<dyn std::error::Error>> {
    if cancelled.is_some_and(|cancelled| cancelled.is_cancelled()) {
        return Ok(Configured {
            path,
            backup: None,
            changed: false,
        });
    }
    let parent = path
        .parent()
        .ok_or("Configuration path has no parent directory")?;
    fs::create_dir_all(parent)?;
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = path
        .file_name()
        .ok_or("Configuration path has no filename")?
        .to_string_lossy();
    let backup = previous.as_ref().map(|_| {
        parent.join(format!(
            "{name}.cursor-proxy-{stamp}-{}.bak",
            std::process::id()
        ))
    });
    if let (Some(bytes), Some(backup)) = (&previous, &backup) {
        let mut file = create_file(backup)?;
        file.set_permissions(fs::metadata(&path)?.permissions())?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    let temporary = parent.join(format!(
        ".{name}.cursor-proxy-{stamp}-{}.tmp",
        std::process::id()
    ));
    let mut file = create_file(&temporary)?;
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        if previous.is_some() {
            file.set_permissions(fs::metadata(&path)?.permissions())?;
        }
        file.write_all(output.as_bytes())?;
        file.sync_all()?;
        drop(file);
        match &previous {
            Some(bytes) if fs::read(&path)? != *bytes => {
                return Err("Configuration changed during merge; retry".into());
            }
            None if fs::symlink_metadata(&path).is_ok() => {
                return Err("Configuration was created during merge; retry".into());
            }
            _ => (),
        }
        if cancelled.is_some_and(|cancelled| cancelled.is_cancelled()) {
            return Err("Context publication cancelled".into());
        }
        fs::rename(&temporary, &path)?;
        Ok(())
    })();
    if result.is_err()
        && let Err(error) = fs::remove_file(&temporary)
        && error.kind() != ErrorKind::NotFound
    {
        eprintln!(
            "Cannot remove temporary configuration {}: {error}",
            temporary.display()
        );
    }
    result?;
    Ok(Configured {
        path,
        backup,
        changed: true,
    })
}
