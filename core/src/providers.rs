//! The models Maverick can use, through a command line tool the user is signed in to or an API key.
//!
//! Every model call in Maverick asks for one JSON object that matches a schema. Each provider
//! gets there its own way, and `run` hides the difference. The user picks a provider and model
//! for each job under Connections; without a choice, Claude Code with Sonnet runs.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::credentials::{self, Password};
use crate::http;

const TIME_LIMIT: Duration = Duration::from_secs(120);
/// The Keychain items that hold API keys, one per provider.
pub const KEY_SERVICE: &str = "dev.maverick.api-key";
const ANTHROPIC_VERSION: &str = "2026-10-21";

/// Where a model runs.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    ClaudeCode,
    Codex,
    OpenCode,
    Anthropic,
    OpenAi,
    OpenRouter,
}

impl Provider {
    pub const ALL: [Provider; 6] = [
        Provider::ClaudeCode,
        Provider::Codex,
        Provider::OpenCode,
        Provider::Anthropic,
        Provider::OpenAi,
        Provider::OpenRouter,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "Claude Code",
            Provider::Codex => "Codex",
            Provider::OpenCode => "OpenCode",
            Provider::Anthropic => "Anthropic",
            Provider::OpenAi => "OpenAI",
            Provider::OpenRouter => "OpenRouter",
        }
    }

    /// The command for a command line provider, `None` for an API.
    pub fn command(self) -> Option<&'static str> {
        match self {
            Provider::ClaudeCode => Some("claude"),
            Provider::Codex => Some("codex"),
            Provider::OpenCode => Some("opencode"),
            _ => None,
        }
    }

    /// The model used when the user names none. Empty means the command's own default.
    pub fn default_model(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "sonnet",
            Provider::Codex | Provider::OpenCode => "",
            Provider::Anthropic => "claude-opus-5-5",
            Provider::OpenAi => "gpt-5-mini",
            Provider::OpenRouter => "openrouter/auto",
        }
    }

    /// Where the user gets a key or installs the command.
    pub fn help(self) -> &'static str {
        match self {
            Provider::ClaudeCode => "Install Claude Code, then run `claude` once to sign in.",
            Provider::Codex => "Install Codex (npm i -g @openai/codex), then run `codex login`.",
            Provider::OpenCode => {
                "Install OpenCode (curl -fsSL https://opencode.ai/install | bash), then run `opencode auth login`."
            }
            Provider::Anthropic => "Create a key at platform.claude.com, under API keys.",
            Provider::OpenAi => "Create a key at platform.openai.com, under API keys.",
            Provider::OpenRouter => "Create a key at openrouter.ai, under Keys.",
        }
    }
}

/// What a model call is for. Each job can use its own provider and model.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "kebab-case")]
pub enum Job {
    Mav,
    Forms,
    Scoring,
}

impl Job {
    pub const ALL: [Job; 3] = [Job::Mav, Job::Forms, Job::Scoring];

    pub fn name(self) -> &'static str {
        match self {
            Job::Mav => "Mav chat",
            Job::Forms => "Form filling",
            Job::Scoring => "Scoring",
        }
    }
}

/// The provider and model for one job.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct Choice {
    pub provider: Provider,
    /// Empty means the provider's default model.
    #[serde(default)]
    pub model: String,
}

impl Choice {
    pub fn model(&self) -> &str {
        if self.model.trim().is_empty() {
            self.provider.default_model()
        } else {
            self.model.trim()
        }
    }

    /// `Claude Code · sonnet`, or just the provider when it uses its own default.
    pub fn label(&self) -> String {
        match self.model() {
            "" => self.provider.name().to_owned(),
            model => format!("{} · {model}", self.provider.name()),
        }
    }
}

impl Default for Choice {
    fn default() -> Self {
        Choice {
            provider: Provider::ClaudeCode,
            model: String::new(),
        }
    }
}

/// The choice for each job, saved as `models.json` in the data directory.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct Choices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mav: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forms: Option<Choice>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scoring: Option<Choice>,
}

pub fn choices_path() -> Option<PathBuf> {
    crate::data_dir().map(|dir| dir.join("models.json"))
}

impl Choices {
    pub fn load(file: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(file) {
            Ok(text) => {
                serde_json::from_str(&text).map_err(|error| format!("{} did not parse: {error}", file.display()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("could not read {}: {error}", file.display())),
        }
    }

    pub fn save(&self, file: &Path) -> Result<(), String> {
        if let Some(folder) = file.parent() {
            std::fs::create_dir_all(folder)
                .map_err(|error| format!("could not create {}: {error}", folder.display()))?;
        }
        let text = serde_json::to_string_pretty(self).expect("choices always serialize");
        std::fs::write(file, text + "\n").map_err(|error| format!("could not write {}: {error}", file.display()))
    }

    /// The saved choices, or the defaults when the file is missing or broken.
    pub fn current() -> Self {
        choices_path()
            .and_then(|file| Self::load(&file).ok())
            .unwrap_or_default()
    }

    pub fn for_job(&self, job: Job) -> Choice {
        let chosen = match job {
            Job::Mav => &self.mav,
            Job::Forms => &self.forms,
            Job::Scoring => &self.scoring,
        };
        chosen.clone().unwrap_or_default()
    }

    pub fn set(&mut self, job: Job, choice: Choice) {
        let slot = match job {
            Job::Mav => &mut self.mav,
            Job::Forms => &mut self.forms,
            Job::Scoring => &mut self.scoring,
        };
        *slot = Some(choice);
    }
}

/// Runs one model call for `job` whose reply must match `schema`, and returns that reply.
pub async fn run(job: Job, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    run_with(&Choices::current().for_job(job), instructions, request, schema).await
}

pub async fn run_with(choice: &Choice, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let model = choice.model();
    match choice.provider {
        Provider::ClaudeCode => claude_code(model, instructions, request, schema).await,
        Provider::Codex => codex(model, instructions, request, schema).await,
        Provider::OpenCode => open_code(model, instructions, request, schema).await,
        Provider::Anthropic => anthropic(model, instructions, request, schema).await,
        Provider::OpenAi => {
            openai_compatible(
                choice.provider,
                "https://api.openai.com/v1",
                model,
                instructions,
                request,
                schema,
            )
            .await
        }
        Provider::OpenRouter => {
            openai_compatible(
                choice.provider,
                "https://openrouter.ai/api/v1",
                model,
                instructions,
                request,
                schema,
            )
            .await
        }
    }
}

/// Where a command line provider's command is, looking where an app started from the Dock
/// does not, such as `~/.local/bin`.
pub fn find_command(name: &str) -> Option<PathBuf> {
    let home = dirs::home_dir();
    let mut places: Vec<PathBuf> = [".local/bin", ".opencode/bin", ".bun/bin", ".npm-global/bin"]
        .iter()
        .filter_map(|dir| home.as_ref().map(|home| home.join(dir)))
        .collect();
    places.extend(["/opt/homebrew/bin", "/usr/local/bin"].map(PathBuf::from));
    if let Some(path) = std::env::var_os("PATH") {
        places.extend(std::env::split_paths(&path));
    }
    places
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|command| command.is_file())
}

/// Runs `command` with `arguments` in the temporary directory, feeding `input` on standard input.
async fn run_command(
    command: &Path,
    arguments: &[String],
    input: Option<&str>,
) -> Result<std::process::Output, String> {
    let mut child = Command::new(command)
        .args(arguments)
        .current_dir(std::env::temp_dir())
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("could not run {}: {error}", command.display()))?;
    if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
        stdin
            .write_all(input.as_bytes())
            .await
            .map_err(|error| format!("could not talk to {}: {error}", command.display()))?;
    }
    tokio::time::timeout(TIME_LIMIT, child.wait_with_output())
        .await
        .map_err(|_| "the model did not answer in time".to_owned())?
        .map_err(|error| format!("{} failed: {error}", command.display()))
}

fn command_for(provider: Provider) -> Result<PathBuf, String> {
    let name = provider.command().expect("a command line provider");
    find_command(name).ok_or_else(|| format!("{} is not installed. {}", provider.name(), provider.help()))
}

async fn claude_code(model: &str, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let command = command_for(Provider::ClaudeCode)?;
    let arguments: Vec<String> = [
        "--print",
        "--model",
        model,
        "--output-format",
        "json",
        // No tools, no project or user settings, no saved session: one question in, one answer out.
        "--tools",
        "",
        "--strict-mcp-config",
        "--setting-sources",
        "",
        "--no-session-persistence",
        "--disable-slash-commands",
        "--system-prompt",
        instructions,
        "--json-schema",
        &schema.to_string(),
    ]
    .iter()
    .map(|argument| (*argument).to_owned())
    .collect();
    let output = run_command(&command, &arguments, Some(&request)).await?;
    let reply: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        let said = String::from_utf8_lossy(&output.stderr);
        format!("the claude command failed: {}", said.trim())
    })?;
    if reply["is_error"] == true {
        return Err(format!(
            "the model failed: {}",
            reply["result"].as_str().unwrap_or("no reason given")
        ));
    }
    match &reply["structured_output"] {
        Value::Null => Err("Claude Code gave no structured reply".to_owned()),
        answer => Ok(answer.clone()),
    }
}

async fn codex(model: &str, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let command = command_for(Provider::Codex)?;
    let scratch = tempfile::tempdir().map_err(|error| format!("could not make a scratch folder: {error}"))?;
    let schema_file = scratch.path().join("schema.json");
    let answer_file = scratch.path().join("answer.json");
    std::fs::write(&schema_file, schema.to_string()).map_err(|error| format!("could not write the schema: {error}"))?;
    let mut arguments: Vec<String> = vec![
        "exec".into(),
        "--skip-git-repo-check".into(),
        "--ephemeral".into(),
        "--sandbox".into(),
        "read-only".into(),
        "--output-schema".into(),
        schema_file.display().to_string(),
        "--output-last-message".into(),
        answer_file.display().to_string(),
    ];
    if !model.is_empty() {
        arguments.extend(["--model".into(), model.into()]);
    }
    arguments.push("-".into());
    let prompt = format!("{instructions}\n\n{request}");
    let output = run_command(&command, &arguments, Some(&prompt)).await?;
    let answer = std::fs::read_to_string(&answer_file).unwrap_or_default();
    if answer.trim().is_empty() {
        let said = String::from_utf8_lossy(&output.stderr);
        let last = said
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("no reason given");
        return Err(format!("Codex gave no answer: {}", last.trim()));
    }
    serde_json::from_str(answer.trim()).map_err(|_| "Codex's answer was not JSON".to_owned())
}

async fn open_code(model: &str, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let command = command_for(Provider::OpenCode)?;
    // OpenCode has no way to enforce a schema, so it is asked for one, and its reply is checked.
    let prompt = format!(
        "{instructions}\n\n{request}\n\nReply with only one JSON object that matches this JSON schema, \
         with no other text and no code fence:\n{schema}"
    );
    let mut arguments: Vec<String> = vec!["run".into()];
    if !model.is_empty() {
        arguments.extend(["--model".into(), model.into()]);
    }
    let mut last_error = String::new();
    for _ in 0..2 {
        let mut asked = arguments.clone();
        asked.push(prompt.clone());
        let output = run_command(&command, &asked, None).await?;
        let text = String::from_utf8_lossy(&output.stdout);
        match json_object_in(&text) {
            Some(answer) if fits(&answer, schema) => return Ok(answer),
            _ => {
                let said = String::from_utf8_lossy(&output.stderr);
                last_error = format!(
                    "OpenCode did not answer with the agreed JSON: {}",
                    said.lines()
                        .rev()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or(text.trim())
                );
            }
        }
    }
    Err(last_error)
}

/// The first complete JSON object in `text`, ignoring prose and code fences around it.
pub fn json_object_in(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0usize;
    let (mut in_string, mut escaped) = (false, false);
    for (offset, c) in text[start..].char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..=start + offset]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// True when `answer` has every field `schema` requires. A full check is left to the caller,
/// which parses the answer into its own types.
fn fits(answer: &Value, schema: &Value) -> bool {
    schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .all(|field| answer.get(field).is_some())
}

pub async fn stored_key(provider: Provider) -> Result<Option<Password>, String> {
    credentials::secret(KEY_SERVICE, provider.name()).await
}

pub async fn save_key(provider: Provider, key: &Password) -> Result<(), String> {
    credentials::store(KEY_SERVICE, provider.name(), key).await
}

pub async fn forget_key(provider: Provider) -> Result<(), String> {
    credentials::forget(KEY_SERVICE, provider.name()).await
}

async fn key_for(provider: Provider) -> Result<Password, String> {
    stored_key(provider)
        .await?
        .ok_or_else(|| format!("no {} API key is stored. {}", provider.name(), provider.help()))
}

async fn anthropic(model: &str, instructions: &str, request: String, schema: &Value) -> Result<Value, String> {
    let key = key_for(Provider::Anthropic).await?;
    let body = json!({
        "model": model,
        "max_tokens": 16000,
        "system": instructions,
        "messages": [{ "role": "user", "content": request }],
        "output_config": { "format": { "type": "json_schema", "schema": schema } },
    });
    let reply = http::post(
        "Anthropic",
        "https://api.anthropic.com/v1/messages",
        &[("x-api-key", key.reveal()), ("anthropic-version", ANTHROPIC_VERSION)],
        &body,
    )
    .await?;
    anthropic_answer(&reply)
}

/// The JSON answer in a reply from Anthropic's Messages API.
pub fn anthropic_answer(reply: &Value) -> Result<Value, String> {
    if reply["stop_reason"] == "refusal" {
        return Err("the model declined to answer".to_owned());
    }
    let text = reply["content"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|block| block["type"] == "text")
        .and_then(|block| block["text"].as_str())
        .ok_or("Anthropic's reply had no text")?;
    serde_json::from_str(text).map_err(|_| "Anthropic's answer was not JSON".to_owned())
}

async fn openai_compatible(
    provider: Provider,
    base: &str,
    model: &str,
    instructions: &str,
    request: String,
    schema: &Value,
) -> Result<Value, String> {
    let key = key_for(provider).await?;
    let body = json!({
        "model": model,
        "messages": [
            { "role": "system", "content": instructions },
            { "role": "user", "content": request },
        ],
        "response_format": {
            "type": "json_schema",
            "json_schema": { "name": "answer", "schema": schema, "strict": true },
        },
    });
    let authorization = format!("Bearer {}", key.reveal());
    let reply = http::post(
        provider.name(),
        &format!("{base}/chat/completions"),
        &[("Authorization", &authorization)],
        &body,
    )
    .await?;
    chat_answer(provider, &reply)
}

/// The JSON answer in a chat completion from OpenAI or OpenRouter.
pub fn chat_answer(provider: Provider, reply: &Value) -> Result<Value, String> {
    let message = &reply["choices"][0]["message"];
    if let Some(refusal) = message["refusal"].as_str() {
        return Err(format!("the model declined to answer: {refusal}"));
    }
    let text = message["content"]
        .as_str()
        .ok_or_else(|| format!("{}'s reply had no answer", provider.name()))?;
    serde_json::from_str(text)
        .ok()
        .or_else(|| json_object_in(text))
        .ok_or_else(|| format!("{}'s answer was not JSON", provider.name()))
}

/// Whether a provider can run now.
#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// It answered a test, or for an API, accepted the key.
    Ready(String),
    /// Installed or keyed but not tested yet.
    Present(String),
    Missing(String),
    Failed(String),
}

/// A quick look that costs nothing: is the command installed, or is a key stored.
pub async fn look(provider: Provider) -> Status {
    match provider.command() {
        Some(name) => match find_command(name) {
            Some(_) => Status::Present("installed".to_owned()),
            None => Status::Missing("not installed".to_owned()),
        },
        None => match stored_key(provider).await {
            Ok(Some(_)) => Status::Present("key stored".to_owned()),
            Ok(None) => Status::Missing("no key".to_owned()),
            Err(error) => Status::Failed(error),
        },
    }
}

/// A real check: an API is asked to list its models, which costs nothing, and a command line
/// provider answers a tiny question with `model`.
pub async fn check(provider: Provider, model: &str) -> Status {
    let outcome = match provider {
        Provider::Anthropic => match key_for(provider).await {
            Ok(key) => http::get(
                "Anthropic",
                "https://api.anthropic.com/v1/models?limit=1",
                &[("x-api-key", key.reveal()), ("anthropic-version", ANTHROPIC_VERSION)],
            )
            .await
            .map(|_| "key works".to_owned()),
            Err(error) => Err(error),
        },
        Provider::OpenAi | Provider::OpenRouter => match key_for(provider).await {
            Ok(key) => {
                let url = if provider == Provider::OpenAi {
                    "https://api.openai.com/v1/models"
                } else {
                    "https://openrouter.ai/api/v1/key"
                };
                let authorization = format!("Bearer {}", key.reveal());
                http::get(provider.name(), url, &[("Authorization", &authorization)])
                    .await
                    .map(|_| "key works".to_owned())
            }
            Err(error) => Err(error),
        },
        _ => {
            let schema = json!({
                "type": "object",
                "properties": { "ok": { "type": "boolean" } },
                "required": ["ok"],
                "additionalProperties": false,
            });
            let choice = Choice {
                provider,
                model: model.to_owned(),
            };
            run_with(
                &choice,
                "Answer the question.",
                "Is 2 + 2 equal to 4?".to_owned(),
                &schema,
            )
            .await
            .and_then(|answer| match answer["ok"].as_bool() {
                Some(_) => Ok("answered a test".to_owned()),
                None => Err("the test answer was not in the agreed form".to_owned()),
            })
        }
    };
    match outcome {
        Ok(detail) => Status::Ready(detail),
        Err(error) => Status::Failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_job_falls_back_to_claude_code_with_sonnet() {
        let mut choices = Choices::default();
        assert_eq!(choices.for_job(Job::Scoring).label(), "Claude Code · sonnet");
        choices.set(
            Job::Scoring,
            Choice {
                provider: Provider::Codex,
                model: String::new(),
            },
        );
        choices.set(
            Job::Mav,
            Choice {
                provider: Provider::OpenRouter,
                model: " anthropic/claude-sonnet-5.5 ".into(),
            },
        );
        assert_eq!(choices.for_job(Job::Scoring).label(), "Codex");
        assert_eq!(choices.for_job(Job::Mav).model(), "anthropic/claude-sonnet-5.5");
        assert_eq!(choices.for_job(Job::Forms), Choice::default());

        let scratch = tempfile::tempdir().unwrap();
        let file = scratch.path().join("models.json");
        assert_eq!(Choices::load(&file), Ok(Choices::default()));
        choices.save(&file).unwrap();
        assert_eq!(Choices::load(&file), Ok(choices));
        assert!(std::fs::read_to_string(&file).unwrap().contains("\"open-router\""));
    }

    #[test]
    fn a_json_object_is_found_inside_prose_and_fences() {
        let text = "Sure.\n```json\n{\"reply\": \"a {brace} and \\\"quote\\\"\", \"n\": {\"x\": 1}}\n```\nDone.";
        assert_eq!(
            json_object_in(text),
            Some(json!({ "reply": "a {brace} and \"quote\"", "n": { "x": 1 } }))
        );
        assert_eq!(json_object_in("no json here"), None);
        assert_eq!(json_object_in("{ broken"), None);
    }

    #[test]
    fn answers_are_read_from_each_api_reply() {
        let anthropic = json!({ "stop_reason": "end_turn", "content": [
            { "type": "thinking", "thinking": "" },
            { "type": "text", "text": "{\"ok\": true}" }
        ]});
        assert_eq!(anthropic_answer(&anthropic), Ok(json!({ "ok": true })));
        assert!(anthropic_answer(&json!({ "stop_reason": "refusal", "content": [] })).is_err());

        let chat = json!({ "choices": [{ "message": { "content": "{\"ok\": false}" } }] });
        assert_eq!(chat_answer(Provider::OpenAi, &chat), Ok(json!({ "ok": false })));
        let refused = json!({ "choices": [{ "message": { "content": null, "refusal": "no" } }] });
        assert!(
            chat_answer(Provider::OpenRouter, &refused)
                .unwrap_err()
                .contains("declined")
        );
    }

    #[test]
    fn a_reply_missing_a_required_field_does_not_fit() {
        let schema = json!({ "required": ["reply", "proposals"] });
        assert!(fits(&json!({ "reply": "", "proposals": [] }), &schema));
        assert!(!fits(&json!({ "reply": "" }), &schema));
    }
}
