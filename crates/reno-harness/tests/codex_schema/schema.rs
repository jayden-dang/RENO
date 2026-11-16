//! Validation of Codex app-server frames against the wire schema `codex app-server
//! generate-json-schema --out DIR` emits for one CLI version.
//!
//! The directory is `RENO_CODEX_SCHEMA_DIR` when set (a fresh generation, to check the driver
//! against the CLI installed on this machine without a model call), else the snapshot committed
//! under `tests/fixtures/codex/schema` (the version in its `VERSION` file).
//!
//! Frames are checked by method: the schema's union files (`ClientRequest.json`,
//! `ClientNotification.json`, `ServerRequest.json`, `ServerNotification.json`) are a `oneOf` per
//! method, so the matching branch is picked and the frame validated against it alone, which
//! keeps every error readable. The approval and question replies the driver sends are validated
//! against their own `*Response.json`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use jsonschema::Validator;
use serde_json::Value;

/// Where the committed snapshot lives.
pub(crate) fn snapshot_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("codex")
        .join("schema")
}

/// The schema directory in use: `RENO_CODEX_SCHEMA_DIR`, else the snapshot.
pub(crate) fn schema_dir() -> PathBuf {
    std::env::var_os("RENO_CODEX_SCHEMA_DIR").map_or_else(snapshot_dir, PathBuf::from)
}

/// The union file that holds each kind of frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Union {
    ClientRequest,
    ClientNotification,
    ServerRequest,
    ServerNotification,
}

impl Union {
    fn file(self) -> &'static str {
        match self {
            Self::ClientRequest => "ClientRequest.json",
            Self::ClientNotification => "ClientNotification.json",
            Self::ServerRequest => "ServerRequest.json",
            Self::ServerNotification => "ServerNotification.json",
        }
    }
}

/// The reply schema for a server request's method, when the driver answers it with a result.
pub(crate) fn response_file(method: &str) -> Option<&'static str> {
    Some(match method {
        "item/commandExecution/requestApproval" => "CommandExecutionRequestApprovalResponse.json",
        "item/fileChange/requestApproval" => "FileChangeRequestApprovalResponse.json",
        "item/permissions/requestApproval" => "PermissionsRequestApprovalResponse.json",
        "item/tool/requestUserInput" => "ToolRequestUserInputResponse.json",
        "mcpServer/elicitation/request" => "McpServerElicitationRequestResponse.json",
        _ => return None,
    })
}

/// How much of the schema a frame is held to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Strictness {
    /// Everything, `required` included.
    Complete,
    /// Types, enums and variants, but a missing field is not an error.
    Shape,
}

/// Remove every `required` list (an array of names) from a schema. A *property* that happens to
/// be called `required` is an object, and stays.
fn drop_required(schema: &mut Value) {
    match schema {
        Value::Object(map) => {
            if map.get("required").is_some_and(Value::is_array) {
                map.remove("required");
            }
            map.values_mut().for_each(drop_required);
        }
        Value::Array(items) => items.iter_mut().for_each(drop_required),
        _ => {}
    }
}

pub(crate) struct Schemas {
    dir: PathBuf,
    files: HashMap<String, Value>,
    validators: HashMap<String, Validator>,
}

impl Schemas {
    pub(crate) fn open(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
            files: HashMap::new(),
            validators: HashMap::new(),
        }
    }

    fn load(&mut self, file: &str) -> Result<&Value, String> {
        if !self.files.contains_key(file) {
            let path = self.dir.join(file);
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            let value = serde_json::from_str(&text)
                .map_err(|e| format!("{} is not JSON: {e}", path.display()))?;
            self.files.insert(file.to_owned(), value);
        }
        self.files
            .get(file)
            .ok_or_else(|| format!("{file} was not loaded"))
    }

    /// The `oneOf` branch of `union` for `method`, as a schema on its own, with the union's
    /// definitions.
    fn branch(&mut self, union: Union, method: &str) -> Result<Value, String> {
        let root = self.load(union.file())?;
        let variant = root["oneOf"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| {
                v.pointer("/properties/method/enum/0")
                    .and_then(Value::as_str)
                    == Some(method)
            })
            .cloned()
            .ok_or_else(|| format!("{method} is not a {} method in this schema", union.file()))?;
        // The branch merged into the root: validating it directly names the field that is wrong,
        // where a one-branch `oneOf` only says no branch matched.
        let mut schema = root.clone();
        if let (Some(root), Some(branch)) = (schema.as_object_mut(), variant.as_object()) {
            root.remove("oneOf");
            root.remove("description");
            for (key, value) in branch {
                root.insert(key.clone(), value.clone());
            }
        }
        Ok(schema)
    }

    fn validator(
        &mut self,
        key: &str,
        schema: impl FnOnce(&mut Self) -> Result<Value, String>,
    ) -> Result<&Validator, String> {
        if !self.validators.contains_key(key) {
            let schema = schema(self)?;
            let validator = jsonschema::validator_for(&schema)
                .map_err(|e| format!("cannot compile the schema of {key}: {e}"))?;
            self.validators.insert(key.to_owned(), validator);
        }
        self.validators
            .get(key)
            .ok_or_else(|| format!("{key} was not compiled"))
    }

    /// Validate `frame` (a whole JSON-RPC message) as `method` of `union`. Also reports every
    /// key of `params` the schema does not list: a field the driver sends that the CLI would
    /// ignore (or, with strict parsing, refuse).
    pub(crate) fn check(&mut self, union: Union, method: &str, frame: &Value) -> Vec<String> {
        self.check_as(union, method, frame, Strictness::Complete)
    }

    /// [`Self::check`], with how strictly to read the schema. Frames the *driver* sends are
    /// held to the whole schema; frames a *fake* serves are trimmed by hand to the fields the
    /// driver reads, so for those `Shape` skips "required" and keeps every type, enum and
    /// variant rule.
    pub(crate) fn check_as(
        &mut self,
        union: Union,
        method: &str,
        frame: &Value,
        strictness: Strictness,
    ) -> Vec<String> {
        let key = format!("{}#{method}#{strictness:?}", union.file());
        let validator = match self.validator(&key, |s| {
            let mut schema = s.branch(union, method)?;
            if strictness == Strictness::Shape {
                drop_required(&mut schema);
            }
            Ok(schema)
        }) {
            Ok(v) => v,
            Err(e) => return vec![e],
        };
        let mut problems: Vec<String> = validator
            .iter_errors(frame)
            .map(|e| format!("{e} (at {})", e.instance_path()))
            .collect();
        problems.extend(self.unknown_params(union, method, frame));
        problems
    }

    fn unknown_params(&mut self, union: Union, method: &str, frame: &Value) -> Vec<String> {
        let Some(params) = frame.get("params").and_then(Value::as_object) else {
            return Vec::new();
        };
        let Ok(branch) = self.branch(union, method) else {
            return Vec::new();
        };
        let Some(reference) = branch
            .pointer("/properties/params/$ref")
            .and_then(Value::as_str)
        else {
            return Vec::new();
        };
        let Some(name) = reference.strip_prefix("#/definitions/") else {
            return Vec::new();
        };
        let Some(definition) = branch.pointer(&format!("/definitions/{name}")) else {
            return Vec::new();
        };
        // The fields a definition lists itself and those of its `oneOf`/`anyOf`/`allOf` branches
        // (Codex flattens a tagged request into one object with the variants' fields).
        let mut listed: Vec<&str> = Vec::new();
        fn collect<'a>(schema: &'a Value, listed: &mut Vec<&'a str>) {
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                listed.extend(props.keys().map(String::as_str));
            }
        }
        collect(definition, &mut listed);
        for combinator in ["oneOf", "anyOf", "allOf"] {
            for variant in definition
                .get(combinator)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                collect(variant, &mut listed);
            }
        }
        params
            .keys()
            .filter(|key| !listed.contains(&key.as_str()))
            .map(|key| format!("params.{key} is not a field of {name}"))
            .collect()
    }

    /// Validate the `result` the driver sent for a server request of `method`.
    pub(crate) fn check_response(&mut self, method: &str, result: &Value) -> Vec<String> {
        let Some(file) = response_file(method) else {
            return vec![format!("no reply schema is known for {method}")];
        };
        let validator = match self.validator(file, |s| s.load(file).cloned()) {
            Ok(v) => v,
            Err(e) => return vec![e],
        };
        validator
            .iter_errors(result)
            .map(|e| format!("{e} (at {})", e.instance_path()))
            .collect()
    }

    /// The version the snapshot records, when this is the snapshot.
    pub(crate) fn recorded_version(&self) -> Option<String> {
        std::fs::read_to_string(self.dir.join("VERSION"))
            .ok()
            .map(|v| {
                v.lines()
                    .find(|l| !l.starts_with('#'))
                    .unwrap_or("")
                    .trim()
                    .to_owned()
            })
    }
}
