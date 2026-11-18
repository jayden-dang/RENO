//! Definitions: the bots, triggers and policy a person writes as TOML files (ARCHITECTURE
//! sections 3.2, 4, 7, 8 and 9).
//!
//! Everything here is pure: text and values in, values out. Reading the files, watching them and
//! storing versions belong to `renod`, the kernel and the store.
//!
//! # Layout of the definitions directory
//!
//! ```text
//! <root>/                  ~/.reno by default; RENO_DEFS_DIR overrides it
//!   bots/<name>.toml       one BotDef per file
//!   triggers/<name>.toml   one TriggerDef per file
//!   policy.toml            the one PolicyDef (its name is always `policy`)
//! ```
//!
//! The file stem must equal the definition's `name`. [`DefKind::path_for`] and
//! [`DefKind::parse_path`] are the only code that knows this layout.
//!
//! # Two stages
//!
//! 1. **Parse.** TOML text (or a JSON body) becomes a typed definition. Every definition type
//!    denies unknown fields, so a typo is an error and not a silent default. Parsing stops at
//!    the first problem in a file, because serde does.
//! 2. **Validate.** A pure function over the parsed set returns *every* problem at once, each
//!    with a path such as `bots/keeper.toml: executors[2]: unknown executor "x"`. It checks the
//!    rules of each definition and the references between them: a bot's triggers exist, a
//!    trigger's bot exists, executors are in the set the caller passes in.
//!
//! [`load_set`] runs both over a snapshot of files and returns either a [`DefSet`] of
//! [`Validated`] definitions or the whole list of errors. A [`Validated`] value can be made only
//! by that function, so holding one proves the rules were checked.
//!
//! # Wire case
//!
//! The rest of the protocol uses `camelCase`. Definitions use `snake_case` because they are a
//! file format first: the body that `def.get` returns is the same shape as the file, so one
//! spelling serves both. The request and reply types around a body ([`DefSummary`], [`DefRecord`]
//! and so on) follow the protocol's `camelCase`.
//!
//! # Hashing
//!
//! A definition's version is identified by the SHA-256 of its canonical JSON ([`content_hash`]):
//! the typed value serialised with every object's keys sorted and no whitespace. Comments,
//! whitespace, key order and fields left at their defaults therefore never make a version.
//!
//! # What validation does not check
//!
//! - A cron expression is parsed by `croner`, the same parser the scheduler fires from
//!   ([`CronSchedule`]), and must match some date. Nothing is run.
//! - Time zones are looked up in a bundled copy of the IANA database, so the answer does not
//!   depend on the machine.
//! - `team` names are checked for shape only: teams arrive with T2.4.
//! - The bots a policy rule is scoped to are not required to exist, so a policy can outlive a
//!   bot it names.

mod bot;
mod eval;
mod hash;
mod normalize;
mod pipeline;
mod policy;
mod protect;
mod time;
mod trigger;
mod wire;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::ExecutorId;

pub use bot::{BotDef, Budget, ResumePolicy};
pub use eval::{DEFAULT_RULE, EvalContext, PolicyDecision, RuleRef, evaluate};
pub(crate) use eval::{command_holds, lexical, locate_text, path_matches};
pub use hash::{canonical_json, content_hash};
pub(crate) use normalize::{allow_form, deny_forms};
pub use normalize::{shell_join, unwrapped_command};
pub(crate) use pipeline::pure_pipeline_head;
pub use policy::{PolicyDef, PolicyEffect, PolicyRule, RuleMatch, RuleScope};
pub use time::{
    CronSchedule, MIN_EVERY, format_instant, format_span, parse_duration, parse_instant_ms,
};
pub use trigger::{
    LATE_GRACE, MAX_MISSED_COUNT, PROMPT_PLACEHOLDERS, PromptVars, TriggerDef, TriggerKind,
    TriggerTarget, lateness_note, render_prompt,
};
pub use wire::{
    BotPin, DefGetParams, DefListParams, DefRecord, DefSummary, DefValidateParams,
    DefValidateResult, DefsSnapshot, DefsWatchParams,
};

/// The defaults a first start writes: an example bot and the safe policy.
pub mod defaults {
    /// `bots/example.toml`.
    pub const EXAMPLE_BOT: &str = include_str!("defaults/example-bot.toml");
    /// `policy.toml`: the policy table of the prototype as rules, with deny first.
    pub const POLICY: &str = include_str!("defaults/policy.toml");
}

/// The longest a definition name may be.
pub const MAX_NAME_LEN: usize = 64;
/// The largest definition file the loaders read, in bytes.
pub const MAX_FILE_BYTES: usize = 256 * 1024;
/// The most files of one kind a loader reads.
pub const MAX_FILES_PER_KIND: usize = 1024;
/// The name of the one policy definition; the file is `policy.toml`.
pub const POLICY_NAME: &str = "policy";
/// The executors Reno knows how to build, installed or not. A definition may name any of them:
/// whether one is installed is the router's question, not the file's.
pub const BUILTIN_EXECUTORS: &[&str] = &[
    "mock",
    "claude-code",
    "codex",
    "acp.grok",
    "acp.devin",
    "acp.hermes",
    "acp.fx",
    "acp.antigravity",
];
/// The namespace of the executors a daemon adds from the ACP registry (`acp.<registry id>`).
pub const ACP_NAMESPACE: &str = "acp.";

/// The kinds of definition this version loads. Teams and flows join with their tasks.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DefKind {
    /// A bot: `bots/<name>.toml`.
    Bot,
    /// A trigger: `triggers/<name>.toml`.
    Trigger,
    /// The policy: `policy.toml`.
    Policy,
}

impl DefKind {
    /// Every kind, in load order: bots and triggers before the policy that scopes to bots.
    pub const ALL: [DefKind; 3] = [Self::Bot, Self::Trigger, Self::Policy];

    /// The name used on the wire and in the `definitions.kind` column.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Bot => "bot",
            Self::Trigger => "trigger",
            Self::Policy => "policy",
        }
    }

    /// The path of a definition's file relative to the definitions root.
    pub fn path_for(self, name: &str) -> String {
        match self {
            Self::Bot => format!("bots/{name}.toml"),
            Self::Trigger => format!("triggers/{name}.toml"),
            Self::Policy => format!("{POLICY_NAME}.toml"),
        }
    }

    /// The kind and name a root-relative path (with `/` separators) belongs to, or `None` when
    /// it is not a definition file.
    pub fn parse_path(path: &str) -> Option<(Self, String)> {
        if path == "policy.toml" {
            return Some((Self::Policy, POLICY_NAME.to_owned()));
        }
        let (dir, file) = path.split_once('/')?;
        let stem = file.strip_suffix(".toml")?;
        if stem.is_empty() || stem.contains('/') {
            return None;
        }
        match dir {
            "bots" => Some((Self::Bot, stem.to_owned())),
            "triggers" => Some((Self::Trigger, stem.to_owned())),
            _ => None,
        }
    }
}

impl fmt::Display for DefKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for DefKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == s)
            .ok_or_else(|| format!("`{s}` is not a definition kind (bot, trigger or policy)"))
    }
}

/// One definition file as read from disk: which kind it is, the name from its file stem, and its
/// text. The loader has already checked that the text is UTF-8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefFile {
    /// What the file defines.
    pub kind: DefKind,
    /// The file stem (`keeper` for `bots/keeper.toml`; `policy` for `policy.toml`).
    pub name: String,
    /// The file's contents.
    pub text: String,
}

impl DefFile {
    /// The file's path relative to the definitions root, as errors show it.
    pub fn path(&self) -> String {
        self.kind.path_for(&self.name)
    }
}

/// One problem with a definition, located by file and by a path inside it.
///
/// Displays as `bots/keeper.toml: executors[2]: unknown executor "x"`. A problem with the file
/// as a whole has an empty `path` and displays as `bots/keeper.toml: <message>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefError {
    /// The file, relative to the definitions root.
    pub file: String,
    /// The field inside it, such as `executors[2]` or `target.bot`; empty for the whole file.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// What is wrong, for a person.
    pub message: String,
}

impl DefError {
    /// An error at `path` inside `file`.
    pub fn new(
        file: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            file: file.into(),
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for DefError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            write!(f, "{}: {}", self.file, self.message)
        } else {
            write!(f, "{}: {}: {}", self.file, self.path, self.message)
        }
    }
}

/// What validation needs to know about the world outside the files.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationContext {
    /// The executor ids a bot may list.
    pub executors: BTreeSet<ExecutorId>,
    /// Whether any `acp.<registry id>` also counts. A daemon knows which registry agents this
    /// device has added; a check with no daemon cannot, so it takes the whole namespace.
    pub any_acp: bool,
}

impl ValidationContext {
    /// A context that knows exactly `executors`.
    pub fn new(executors: impl IntoIterator<Item = ExecutorId>) -> Self {
        Self {
            executors: executors.into_iter().collect(),
            any_acp: false,
        }
    }

    /// [`Self::with_builtin`] for a check that has no daemon to ask: every `acp.<registry id>`
    /// is accepted too, as the daemon accepts the registry agents it has added.
    pub fn offline(extra: impl IntoIterator<Item = ExecutorId>) -> Self {
        Self {
            any_acp: true,
            ..Self::with_builtin(extra)
        }
    }

    /// Whether a bot may list `id`.
    pub fn knows(&self, id: &ExecutorId) -> bool {
        self.executors.contains(id) || (self.any_acp && id.as_str().starts_with(ACP_NAMESPACE))
    }

    /// A context that knows [`BUILTIN_EXECUTORS`] and `extra`.
    pub fn with_builtin(extra: impl IntoIterator<Item = ExecutorId>) -> Self {
        let builtin = BUILTIN_EXECUTORS.iter().filter_map(|id| id.parse().ok());
        Self::new(builtin.chain(extra))
    }
}

/// A definition that passed validation, with its canonical form and hash. Only [`load_set`] and
/// its siblings make one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Validated<T> {
    def: T,
    body: Value,
    canonical: String,
    hash: String,
}

impl<T: Serialize> Validated<T> {
    fn seal(def: T) -> Result<Self, String> {
        let body = serde_json::to_value(&def).map_err(|e| e.to_string())?;
        let canonical = canonical_json(&body);
        let hash = content_hash(&body);
        Ok(Self {
            def,
            body,
            canonical,
            hash,
        })
    }
}

impl<T> Validated<T> {
    /// The definition.
    pub fn def(&self) -> &T {
        &self.def
    }

    /// The definition as JSON: the `body` that is stored and that `def.get` returns.
    pub fn body(&self) -> &Value {
        &self.body
    }

    /// The canonical JSON text the hash is taken over.
    pub fn canonical(&self) -> &str {
        &self.canonical
    }

    /// The SHA-256 of [`canonical`](Self::canonical), as 64 lowercase hex digits.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

impl<T> std::ops::Deref for Validated<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.def
    }
}

/// One definition of a [`DefSet`], as the store needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DefEntry<'a> {
    /// Its kind.
    pub kind: DefKind,
    /// Its name.
    pub name: &'a str,
    /// Its content hash.
    pub hash: &'a str,
    /// Its canonical JSON.
    pub canonical: &'a str,
    /// Its JSON body: the same value as `canonical`, parsed.
    pub body: &'a Value,
}

/// Every definition of a snapshot, validated together. The empty set is valid.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefSet {
    bots: BTreeMap<String, Validated<BotDef>>,
    triggers: BTreeMap<String, Validated<TriggerDef>>,
    policy: Option<Validated<PolicyDef>>,
}

impl DefSet {
    /// The bots, by name.
    pub fn bots(&self) -> &BTreeMap<String, Validated<BotDef>> {
        &self.bots
    }

    /// One bot.
    pub fn bot(&self, name: &str) -> Option<&Validated<BotDef>> {
        self.bots.get(name)
    }

    /// The triggers, by name.
    pub fn triggers(&self) -> &BTreeMap<String, Validated<TriggerDef>> {
        &self.triggers
    }

    /// The policy, when `policy.toml` exists. Without one, enforcement (T1.6) decides what
    /// applies.
    pub fn policy(&self) -> Option<&Validated<PolicyDef>> {
        self.policy.as_ref()
    }

    /// How many definitions the set holds.
    pub fn len(&self) -> usize {
        self.bots.len() + self.triggers.len() + usize::from(self.policy.is_some())
    }

    /// Whether the set holds nothing.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Every definition, ordered by kind and then name.
    pub fn entries(&self) -> Vec<DefEntry<'_>> {
        let bots = self.bots.iter().map(|(name, v)| DefEntry {
            kind: DefKind::Bot,
            name,
            hash: v.hash(),
            canonical: v.canonical(),
            body: v.body(),
        });
        let triggers = self.triggers.iter().map(|(name, v)| DefEntry {
            kind: DefKind::Trigger,
            name,
            hash: v.hash(),
            canonical: v.canonical(),
            body: v.body(),
        });
        let policy = self.policy.iter().map(|v| DefEntry {
            kind: DefKind::Policy,
            name: POLICY_NAME,
            hash: v.hash(),
            canonical: v.canonical(),
            body: v.body(),
        });
        bots.chain(triggers).chain(policy).collect()
    }
}

/// Loads a snapshot of definition files: parses each, then validates the whole set.
///
/// Returns every error found, grouped by file in load order (bots, triggers, policy, each by
/// name), or the validated set. No partial set is ever returned: a set with one bad file is not loaded.
pub fn load_set(files: &[DefFile], ctx: &ValidationContext) -> Result<DefSet, Vec<DefError>> {
    let mut parsed = Parsed::default();
    let mut errors = Vec::new();
    for file in files {
        parsed.add_file(file, &mut errors);
    }
    finish(parsed, errors, ctx)
}

/// Rebuilds a set from stored JSON bodies, for example the last good versions in the store.
/// Bodies go through the same validation as files, so a body that no longer passes (an executor
/// that has gone) is reported rather than trusted.
pub fn load_bodies(
    bodies: &[(DefKind, String, Value)],
    ctx: &ValidationContext,
) -> Result<DefSet, Vec<DefError>> {
    let mut parsed = Parsed::default();
    let mut errors = Vec::new();
    for (kind, name, body) in bodies {
        parsed.add_body(*kind, name, body, &mut errors);
    }
    finish(parsed, errors, ctx)
}

/// Validates one JSON body as if it were saved as `name`, in the context of the `current` set,
/// and returns the errors that concern it: the same checks a file gets, including references to
/// and from the other definitions. Nothing is saved. An empty list means the body is valid.
pub fn validate_body(
    current: &DefSet,
    kind: DefKind,
    name: &str,
    body: &Value,
    ctx: &ValidationContext,
) -> (Vec<DefError>, Option<String>) {
    let mut parsed = Parsed::from_set(current);
    parsed.remove(kind, name);
    let mut errors = Vec::new();
    let file = kind.path_for(name);
    parsed.add_body(kind, name, body, &mut errors);
    errors.extend(parsed.check(ctx).into_iter().filter(|e| e.file == file));
    if !errors.is_empty() {
        return (errors, None);
    }
    let sealed = match kind {
        DefKind::Bot => parsed
            .bots
            .remove(name)
            .map(|d| Validated::seal(d).map(|v| v.hash)),
        DefKind::Trigger => parsed
            .triggers
            .remove(name)
            .map(|d| Validated::seal(d).map(|v| v.hash)),
        DefKind::Policy => parsed
            .policy
            .take()
            .map(|d| Validated::seal(d).map(|v| v.hash)),
    };
    match sealed {
        Some(Ok(hash)) => (Vec::new(), Some(hash)),
        Some(Err(message)) => (vec![DefError::new(file, "", message)], None),
        None => (Vec::new(), None),
    }
}

fn finish(
    parsed: Parsed,
    mut errors: Vec<DefError>,
    ctx: &ValidationContext,
) -> Result<DefSet, Vec<DefError>> {
    errors.extend(parsed.check(ctx));
    if !errors.is_empty() {
        sort_errors(&mut errors);
        return Err(errors);
    }
    parsed.seal().map_err(|e| vec![e])
}

/// Groups errors by file, in load order (bots, triggers, policy, each by name), keeping the order
/// within a file. Errors about something that is not a definition file go last.
fn sort_errors(errors: &mut [DefError]) {
    errors.sort_by_cached_key(|e| match DefKind::parse_path(&e.file) {
        Some((kind, name)) => (0, kind, name),
        None => (1, DefKind::Bot, e.file.clone()),
    });
}

/// Parsed but not yet validated definitions.
#[derive(Debug, Default)]
struct Parsed {
    bots: BTreeMap<String, BotDef>,
    triggers: BTreeMap<String, TriggerDef>,
    policy: Option<PolicyDef>,
    /// Definitions that exist but did not parse, so a reference to one is not also reported as
    /// missing.
    unparsed: BTreeSet<(DefKind, String)>,
}

impl Parsed {
    fn from_set(set: &DefSet) -> Self {
        Self {
            bots: set
                .bots
                .iter()
                .map(|(k, v)| (k.clone(), v.def.clone()))
                .collect(),
            triggers: set
                .triggers
                .iter()
                .map(|(k, v)| (k.clone(), v.def.clone()))
                .collect(),
            policy: set.policy.as_ref().map(|v| v.def.clone()),
            unparsed: BTreeSet::new(),
        }
    }

    fn remove(&mut self, kind: DefKind, name: &str) {
        match kind {
            DefKind::Bot => {
                self.bots.remove(name);
            }
            DefKind::Trigger => {
                self.triggers.remove(name);
            }
            DefKind::Policy => self.policy = None,
        }
    }

    fn add_file(&mut self, file: &DefFile, errors: &mut Vec<DefError>) {
        let path = file.path();
        if file.text.len() > MAX_FILE_BYTES {
            errors.push(DefError::new(
                path,
                "",
                format!(
                    "the file is {} bytes; the limit is {MAX_FILE_BYTES}",
                    file.text.len()
                ),
            ));
            self.unparsed.insert((file.kind, file.name.clone()));
            return;
        }
        if file.kind == DefKind::Policy && file.name != POLICY_NAME {
            errors.push(DefError::new(
                path,
                "",
                "the policy file must be named policy.toml",
            ));
            return;
        }
        match file.kind {
            DefKind::Bot => self.insert_parsed(file, parse_toml::<BotDef>(&file.text), errors),
            DefKind::Trigger => {
                self.insert_parsed(file, parse_toml::<TriggerDef>(&file.text), errors);
            }
            DefKind::Policy => {
                self.insert_parsed(file, parse_toml::<PolicyDef>(&file.text), errors)
            }
        }
    }

    fn add_body(&mut self, kind: DefKind, name: &str, body: &Value, errors: &mut Vec<DefError>) {
        let path = kind.path_for(name);
        let file = DefFile {
            kind,
            name: name.to_owned(),
            text: String::new(),
        };
        match kind {
            DefKind::Bot => self.insert_parsed(&file, parse_json::<BotDef>(body), errors),
            DefKind::Trigger => self.insert_parsed(&file, parse_json::<TriggerDef>(body), errors),
            DefKind::Policy if name == POLICY_NAME => {
                self.insert_parsed(&file, parse_json::<PolicyDef>(body), errors);
            }
            DefKind::Policy => errors.push(DefError::new(
                path,
                "",
                "the policy file must be named policy.toml",
            )),
        }
    }

    fn insert_parsed<T: Definition>(
        &mut self,
        file: &DefFile,
        parsed: Result<T, String>,
        errors: &mut Vec<DefError>,
    ) {
        let path = file.path();
        match parsed {
            Ok(def) => {
                if !def.insert_into(self, &file.name) {
                    errors.push(DefError::new(path, "", "this definition appears twice"));
                }
            }
            Err(message) => {
                errors.push(DefError::new(path, "", message));
                self.unparsed.insert((file.kind, file.name.clone()));
            }
        }
    }

    fn check(&self, ctx: &ValidationContext) -> Vec<DefError> {
        let cx = Cx {
            ctx,
            bots: &self.bots,
            triggers: &self.triggers,
            unparsed: &self.unparsed,
        };
        let mut errors = Vec::new();
        for (stem, bot) in &self.bots {
            bot.validate(stem, &cx, &mut Sink::new(DefKind::Bot, stem, &mut errors));
        }
        for (stem, trigger) in &self.triggers {
            trigger.validate(
                stem,
                &cx,
                &mut Sink::new(DefKind::Trigger, stem, &mut errors),
            );
        }
        if let Some(policy) = &self.policy {
            policy.validate(&mut Sink::new(DefKind::Policy, POLICY_NAME, &mut errors));
        }
        errors
    }

    fn seal(self) -> Result<DefSet, DefError> {
        let mut set = DefSet::default();
        for (name, def) in self.bots {
            let sealed = seal_def(DefKind::Bot, &name, def)?;
            set.bots.insert(name, sealed);
        }
        for (name, def) in self.triggers {
            let sealed = seal_def(DefKind::Trigger, &name, def)?;
            set.triggers.insert(name, sealed);
        }
        if let Some(def) = self.policy {
            set.policy = Some(seal_def(DefKind::Policy, POLICY_NAME, def)?);
        }
        Ok(set)
    }
}

fn seal_def<T: Serialize>(kind: DefKind, name: &str, def: T) -> Result<Validated<T>, DefError> {
    Validated::seal(def).map_err(|message| DefError::new(kind.path_for(name), "", message))
}

/// A definition type: knows where it sits in a [`Parsed`].
trait Definition: DeserializeOwned {
    /// Adds `self` under `name`. `false` when that name was already taken.
    fn insert_into(self, parsed: &mut Parsed, name: &str) -> bool;
}

impl Definition for BotDef {
    fn insert_into(self, parsed: &mut Parsed, name: &str) -> bool {
        parsed.bots.insert(name.to_owned(), self).is_none()
    }
}

impl Definition for TriggerDef {
    fn insert_into(self, parsed: &mut Parsed, name: &str) -> bool {
        parsed.triggers.insert(name.to_owned(), self).is_none()
    }
}

impl Definition for PolicyDef {
    fn insert_into(self, parsed: &mut Parsed, _name: &str) -> bool {
        parsed.policy.replace(self).is_none()
    }
}

/// Parses TOML into `T`; the error says where, as `line 3, column 5: unknown field ...`.
fn parse_toml<T: DeserializeOwned>(text: &str) -> Result<T, String> {
    toml::from_str(text).map_err(|e| match e.span() {
        Some(span) => {
            let before = &text[..span.start.min(text.len())];
            let line = before.matches('\n').count() + 1;
            let column = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
            format!("line {line}, column {column}: {}", e.message())
        }
        None => e.message().to_owned(),
    })
}

/// Parses a JSON body into `T`.
fn parse_json<T: DeserializeOwned>(body: &Value) -> Result<T, String> {
    T::deserialize(body).map_err(|e| e.to_string())
}

/// What a definition's rules may look at besides itself.
struct Cx<'a> {
    ctx: &'a ValidationContext,
    bots: &'a BTreeMap<String, BotDef>,
    triggers: &'a BTreeMap<String, TriggerDef>,
    unparsed: &'a BTreeSet<(DefKind, String)>,
}

impl Cx<'_> {
    /// Whether a bot of that name exists, parsed or not.
    fn bot_exists(&self, name: &str) -> bool {
        self.bots.contains_key(name) || self.unparsed.contains(&(DefKind::Bot, name.to_owned()))
    }

    fn trigger_exists(&self, name: &str) -> bool {
        self.triggers.contains_key(name)
            || self.unparsed.contains(&(DefKind::Trigger, name.to_owned()))
    }
}

/// Collects the errors of one file.
struct Sink<'a> {
    file: String,
    out: &'a mut Vec<DefError>,
}

impl<'a> Sink<'a> {
    fn new(kind: DefKind, name: &str, out: &'a mut Vec<DefError>) -> Self {
        Self {
            file: kind.path_for(name),
            out,
        }
    }

    fn err(&mut self, path: impl Into<String>, message: impl Into<String>) {
        self.out
            .push(DefError::new(self.file.clone(), path, message));
    }
}

/// Whether `s` is a legal definition name: 1 to [`MAX_NAME_LEN`] of `a-z`, `0-9`, `-`, `_`,
/// starting with a letter or digit. Names are file stems and appear in ids, so they are plain.
pub fn is_valid_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    s.len() <= MAX_NAME_LEN
        && (first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

const NAME_RULE: &str =
    "use 1 to 64 characters from a-z, 0-9, `-` and `_`, starting with a letter or digit";

/// Checks a `name` field: legal, and equal to the file stem.
fn check_name(sink: &mut Sink<'_>, name: &str, stem: &str) {
    if !is_valid_name(name) {
        sink.err("name", format!("{name:?} is not a valid name: {NAME_RULE}"));
    } else if name != stem {
        sink.err(
            "name",
            format!("{name:?} must equal the file name {stem:?}"),
        );
    }
}

/// Checks a reference-by-name field such as `team` or `triggers[0]`.
fn check_ref_name(sink: &mut Sink<'_>, path: &str, name: &str) -> bool {
    let ok = is_valid_name(name);
    if !ok {
        sink.err(path, format!("{name:?} is not a valid name: {NAME_RULE}"));
    }
    ok
}

#[cfg(test)]
mod tests;
