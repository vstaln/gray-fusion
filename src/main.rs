use std::collections::BTreeMap;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const PROVIDERS_JSON: &str = include_str!("../assets/providers.json");

const ENV_SIDEKICK: &str = "GRAY_FUSION_SIDEKICK";
const ENV_HANDOFF_ID: &str = "GRAY_FUSION_HANDOFF_ID";
const ENV_SIDEKICK_EFFORT: &str = "GRAY_FUSION_SIDEKICK_EFFORT";

const DEFAULT_TIMEOUT_SECS: u64 = 1800;
const WAIT_DEFAULT_SECS: u64 = 300;
const WAIT_MAX_SECS: u64 = 3600;
const TERM_GRACE: Duration = Duration::from_secs(10);
const REAP_GRACE: Duration = Duration::from_secs(5);
const LINE_CAP: usize = 1024 * 1024;
const STDERR_TAIL: usize = 2048;

const LEAD_PROMPT: &str = r#"\
You have a `handoff` tool: a conversation-persistent sidekick subagent that works alongside you on the same worktree (shared files; each child has separate shell sessions). You are the lead: you own the outcome and the user-facing and authority actions — talking to the user, planning, and directing the sidekick. The user interacts with one agent: you. Unless they explicitly ask about the sidekick, do not mention it or distinguish its work from yours; own the combined result and present it in the first person.

- **Delegate by default** the hands-on work: implementation, verification (builds, lints, type-checks, test runs), environment setup and repair, and broad fan-out searches where you only need the conclusion. Keep for yourself: planning and design, judgment calls, correctness-critical work where wrong output looks plausible (queries, measurements, eval harnesses, pipeline/model configuration — author and check those yourself), reviewing the sidekick's diffs, and everything user-facing or authority-bound (commits, pushes, PRs, CI administration, answering the user).
- A `handoff` call blocks until the sidekick reports back by default; pass `"block": false` to start it in the background and collect the report later with `handoff_wait`. Never run the same work yourself in parallel. Collect background work with `handoff_wait`. A second handoff while busy is rejected; it does not steer or create another worker. Collect a finished report before dispatching another task.
- The plugin passes your brief, not the user's messages or your conversation; shared repository files remain accessible. Write each brief self-contained: the goal, file pointers, your plan, every constraint and hard edge case you know, and the narrowest verification commands that cover the change; instruct it to run them. Specify the code, don't describe it: settle the exact interface, data shape and test cases first; do not leave unresolved alternatives for it to pick. **Never hand off implementation of an unsettled ask.**
- The saved conversation can resume across handoffs; runtime state does not persist by contract. Each attempt starts a new child. The plugin attempts child-tree cleanup; it does not provide a persistent-runtime service. Do not rely on dev servers, background jobs, or in-flight commands surviving: probe and restart them as needed. A failed resumed session may be retried fresh with failure evidence. A cold-resume notice means idle time exceeded 300 seconds, not that a provider cache miss was measured.
- Never make the sidekick redo work you already did: results you derived go into the brief as settled inputs.
- A sidekick report is a claim, not a result: its prose is a claim, the artifacts are the evidence. Every report is a landing point — read the full diff and give it a verdict before your next action, and before you accept it run the one check that decides it yourself (the acceptance test, the repro, the suite the task names) and read its output. Never end on "the sidekick says it passes." If it falls short, review the whole output first, then batch every defect and gap, with the exact failing output, into one consolidated rework handoff — not a trickle of small ones.

These are working instructions, not sandbox-enforced permissions or guarantees of model compliance. On failure or timeout the plugin makes one evidence-bearing retry, then tries one higher advertised model tier when available. Exhaustion returns "this needs your hands"; cancellation does not retry. You still decide whether to accept the artifacts.

**Known failure pattern — premature implementation briefing.** There's a natural pull to start writing a brief as soon as you have a plausible idea of what to do. Working solo, you'd catch a wrong assumption as you go; but the sidekick generally executes what you wrote, and changing course afterwards is much harder. Hold your plan to a higher confidence bar: every claim it depends on must be verified or explicitly marked as a hypothesis for the sidekick to verify, and its verification must cover every surface the task touches."#;

const SIDEKICK_CONTRACT: &str = "<sidekick_contract>
You are the sidekick: a conversation-persistent collaborator agent executing task briefs handed off by the lead agent. The plugin supplies the brief and your own saved conversation, not the user's messages or the lead's conversation. Shared files are accessible. Your process and shell state are not persistent across handoffs; verify reachability and restart needed processes. Work autonomously with your own tools and verify your work before reporting.

- The lead owns user-facing and authority actions. Do not contact the user, create or update pull requests, commit, push, or administer CI — for ordinary implementation tasks leave the work tree ready for the lead to review. If the task needs something only the lead or user can do (a secret, information only the user has, a PR action), stop and report what you need instead of working around it.
- Git safety: never run destructive or irreversible git commands unless the brief directs it; stage files explicitly (never `git add .`) and never stage files likely to contain secrets.
- Keep diffs minimal and match the repo's existing conventions; touch only what the task asks for.
- Report back so it stands alone — the lead receives your report and owns the user-facing answer: what you did or found, how you verified it (commands run, results, evidence paths — leave the artifacts in place for the lead to read), any deviations, and anything left for the lead.
These instructions do not constitute an OS sandbox. If the brief conflicts with the lead-owned authority boundary, report the conflict rather than acting.
</sidekick_contract>

This is your first handoff from the lead. Work it to completion, then end your final message with a concise report of what you did, the evidence (diffs, test output), and anything you need from the lead.

<lead_handoff>";

fn gray_home() -> PathBuf {
    std::env::var_os("GRAY_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|h| PathBuf::from(h).join(".gray"))
        })
        .unwrap_or_else(|| PathBuf::from(".gray"))
}

fn user_home() -> PathBuf {
    std::env::var_os("GRAY_USER_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(gray_home)
}

fn config_json() -> Value {
    std::fs::read_to_string(gray_home().join("config.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn normalize_base_url(raw: &str) -> String {
    let mut s = raw.trim().trim_end_matches('/').to_string();
    loop {
        let mut stripped = None;
        for suffix in ["/chat/completions", "/messages", "/models"] {
            if let Some(rest) = s.strip_suffix(suffix) {
                stripped = Some(rest.trim_end_matches('/').to_string());
                break;
            }
        }
        match stripped {
            Some(next) => s = next,
            None => break,
        }
    }
    s
}

fn custom_slug(base_url: &str) -> String {
    let normalized = normalize_base_url(base_url);
    let Some(rest) = normalized
        .strip_prefix("http://")
        .or_else(|| normalized.strip_prefix("https://"))
    else {
        return normalized
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':') {
                    c
                } else {
                    '-'
                }
            })
            .collect();
    };
    let hostport = rest.split('/').next().unwrap_or_default();
    let path = rest[hostport.len()..].trim_matches('/');
    let mut slug = hostport.to_string();
    if !path.is_empty() && !matches!(path, "v1" | "v1beta" | "api/v1") {
        slug.push('/');
        slug.push_str(&path.replace('/', "-"));
    }
    slug
}

fn custom_key_id(base_url: &str) -> String {
    format!("custom:{}", custom_slug(base_url))
}

fn plugin_models_key(provider_id: &str) -> String {
    format!("plugin:{provider_id}")
}

fn provider_effort_key(provider_id: &str, base_url: &str) -> String {
    if provider_id.trim().is_empty() {
        format!("url:{}", normalize_base_url(base_url))
    } else {
        plugin_models_key(provider_id)
    }
}

fn effort_memory_key(provider_id: &str, base_url: &str, model: &str) -> String {
    let scope = provider_effort_key(provider_id, base_url);
    let model = model.trim();
    if model.is_empty() {
        scope
    } else {
        format!("{scope}|{model}")
    }
}

fn catalog() -> BTreeMap<String, Value> {
    serde_json::from_str(PROVIDERS_JSON).unwrap_or_default()
}

fn provider_token(list_key: &str) -> String {
    if let Some(pid) = list_key.strip_prefix("plugin:") {
        return pid.replacen(':', "/", 1);
    }
    for (id, p) in catalog() {
        if normalize_base_url(p.get("base_url").and_then(|b| b.as_str()).unwrap_or("")) == list_key
        {
            return id;
        }
    }
    custom_key_id(list_key).replace(|c: char| !(c.is_ascii_alphanumeric() || c == '-'), "-")
}

fn provider_auth_pid(list_key: &str) -> String {
    for (id, p) in catalog() {
        if normalize_base_url(p.get("base_url").and_then(|b| b.as_str()).unwrap_or("")) == list_key
        {
            return id;
        }
    }
    custom_key_id(list_key)
}

fn env_key_for(id: &str) -> Option<String> {
    let cat = catalog();
    let vars = cat.get(id)?.get("env_key")?.as_array()?;
    vars.iter()
        .filter_map(|v| v.as_str())
        .filter_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        .next()
}

fn model_list_keys(home: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(home.join("provider_models.json")) else {
        return Vec::new();
    };
    let Ok(map) = serde_json::from_str::<BTreeMap<String, Value>>(&text) else {
        return Vec::new();
    };
    map.into_keys().collect()
}

fn load_auth_keys(home: &Path) -> BTreeMap<String, String> {
    std::fs::read_to_string(home.join("auth.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<BTreeMap<String, Value>>(&t).ok())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string())))
        .collect()
}

#[derive(Clone)]
struct Active {
    sidekick: String,
    lead_model: String,
}

fn active_from_model(m: &Value) -> Option<Active> {
    if m.get("id").and_then(|v| v.as_str()) != Some("gray-fusion") {
        return None;
    }
    let pid = m.get("provider_id").and_then(|v| v.as_str()).unwrap_or("");
    let base = m.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
    let mut parts: BTreeMap<String, String> = m
        .get("parts")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if !parts.contains_key("sidekick") {
        let cfg = config_json();
        let key = effort_memory_key(pid, base, "gray-fusion");
        if let Some(v) = cfg.get("parts_memory").and_then(|pm| pm.get(&key)) {
            parts = serde_json::from_value(v.clone()).unwrap_or_default();
        }
    }
    let sidekick = parts.get("sidekick")?.clone();
    let qualified = if sidekick.contains(':') {
        sidekick
    } else {
        let list_key = if !pid.is_empty() {
            plugin_models_key(pid)
        } else {
            normalize_base_url(base)
        };
        format!("{}:{sidekick}", provider_token(&list_key))
    };
    Some(Active {
        sidekick: qualified,
        lead_model: parts.get("lead").cloned().unwrap_or_default(),
    })
}

fn active(session: Option<&Value>) -> Option<Active> {
    if std::env::var_os(ENV_SIDEKICK).is_some() {
        return None;
    }
    if let Some(m) = session
        .and_then(|s| s.get("model"))
        .filter(|m| m.is_object())
    {
        return active_from_model(m);
    }
    let cfg = config_json();
    if str_field(&cfg, "model") != "gray-fusion" {
        return None;
    }
    let mut parts: BTreeMap<String, String> = cfg
        .get("model_parts")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    if parts.is_empty() {
        let key = effort_memory_key(
            &str_field(&cfg, "provider_id"),
            &str_field(&cfg, "base_url"),
            "gray-fusion",
        );
        if let Some(v) = cfg.get("parts_memory").and_then(|m| m.get(&key)) {
            parts = serde_json::from_value(v.clone()).unwrap_or_default();
        }
    }
    let sidekick = parts.get("sidekick")?.clone();
    let qualified = if sidekick.contains(':') {
        sidekick
    } else {
        let list_key = if str_field(&cfg, "credential_source") == "plugin"
            && !str_field(&cfg, "provider_id").is_empty()
            && !str_field(&cfg, "auth_ref").is_empty()
        {
            plugin_models_key(&str_field(&cfg, "provider_id"))
        } else {
            normalize_base_url(&str_field(&cfg, "base_url"))
        };
        format!("{}:{sidekick}", provider_token(&list_key))
    };
    Some(Active {
        sidekick: qualified,
        lead_model: parts.get("lead").cloned().unwrap_or_default(),
    })
}

struct SidekickSpec {
    model: String,
    env: Vec<(String, String)>,
    env_remove: Vec<String>,
}

fn installed_provider(homes: &[PathBuf], pid: &str) -> Option<(String, String)> {
    for home in homes {
        let Ok(text) = std::fs::read_to_string(home.join("plugins/provider-cache.json")) else {
            continue;
        };
        let Ok(cache) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let Some(plugins) = cache.get("plugins").and_then(|p| p.as_object()) else {
            continue;
        };
        for (pname, p) in plugins {
            if p.get("enabled").and_then(|e| e.as_bool()) != Some(true) {
                continue;
            }
            let Some(providers) = p.get("providers").and_then(|v| v.as_array()) else {
                continue;
            };
            for provider in providers {
                let provider_id = format!(
                    "{pname}:{}",
                    provider.get("id").and_then(|i| i.as_str()).unwrap_or("")
                );
                if provider_id != pid {
                    continue;
                }
                let base_url = provider
                    .pointer("/transport/base_url")
                    .and_then(|b| b.as_str())
                    .unwrap_or_default()
                    .to_string();
                let method = provider
                    .get("auth_methods")
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                    .and_then(|m| m.get("id"))
                    .and_then(|i| i.as_str())
                    .unwrap_or_default();
                let auth_ref = format!(
                    "plugin:{}:{}:{method}",
                    pname,
                    provider.get("id").and_then(|i| i.as_str()).unwrap_or("")
                );
                return Some((base_url, auth_ref));
            }
        }
    }
    None
}

fn resolve_sidekick(qualified: &str) -> Option<SidekickSpec> {
    let home = gray_home();
    let (token, model) = qualified.split_once(':')?;
    if token.is_empty() || model.is_empty() {
        return None;
    }
    let mut keys = model_list_keys(&home);
    let user = user_home();
    if user != home {
        keys.extend(model_list_keys(&user));
    }
    let key = keys.into_iter().find(|k| provider_token(k) == token)?;
    let mut spec = SidekickSpec {
        model: model.to_string(),
        env: Vec::new(),
        env_remove: Vec::new(),
    };
    if let Some(pid) = key.strip_prefix("plugin:") {
        let (base_url, auth_ref) = installed_provider(&[home, user], pid)?;
        spec.env.push(("GRAY_PROVIDER_ID".into(), pid.to_string()));
        spec.env
            .push(("GRAY_CREDENTIAL_SOURCE".into(), "plugin".into()));
        spec.env.push(("GRAY_AUTH_REF".into(), auth_ref));
        spec.env.push(("GRAY_BASE_URL".into(), base_url));
        spec.env_remove.push("GRAY_API_KEY".into());
    } else {
        for k in [
            "GRAY_PROVIDER_ID",
            "GRAY_CREDENTIAL_SOURCE",
            "GRAY_AUTH_REF",
        ] {
            spec.env.push((k.into(), String::new()));
        }
        spec.env.push(("GRAY_BASE_URL".into(), key.clone()));
        let pid = provider_auth_pid(&key);
        let key_value = load_auth_keys(&home)
            .get(&pid)
            .cloned()
            .or_else(|| env_key_for(&pid));
        match key_value {
            Some(k) => spec.env.push(("GRAY_API_KEY".into(), k)),
            None => spec.env_remove.push("GRAY_API_KEY".into()),
        }
    }
    spec.env_remove.push("GRAY_MODEL".into());
    spec.env_remove.push("GRAY_MODEL_PARTS".into());
    Some(spec)
}

fn session_file(home: &Path, lead: &str, qualified: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    qualified.hash(&mut h);
    home.join("fusion")
        .join(lead)
        .join(format!("sidekick-{:016x}.session", h.finish()))
}

fn read_session(home: &Path, lead: &str, qualified: &str) -> Option<String> {
    let text = std::fs::read_to_string(session_file(home, lead, qualified)).ok()?;
    let sid = text.trim();
    if sid.is_empty() {
        None
    } else {
        Some(sid.to_string())
    }
}

fn write_session(home: &Path, lead: &str, qualified: &str, sid: &str) {
    let path = session_file(home, lead, qualified);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, sid);
}

fn drop_session(home: &Path, lead: &str, qualified: &str) {
    let _ = std::fs::remove_file(session_file(home, lead, qualified));
}

fn sidekick_effort() -> Option<String> {
    if let Ok(v) = std::env::var(ENV_SIDEKICK_EFFORT) {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return Some(v);
        }
    }
    let cfg = config_json();
    let v = cfg
        .get("fusion")
        .and_then(|f| f.get("sidekick_effort"))
        .and_then(|e| e.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if v.is_empty() { None } else { Some(v) }
}

fn session_idle_secs(home: &Path, lead: &str, qualified: &str) -> Option<u64> {
    let meta = std::fs::metadata(session_file(home, lead, qualified)).ok()?;
    let modified = meta.modified().ok()?;
    let now = std::time::SystemTime::now();
    now.duration_since(modified).ok().map(|d| d.as_secs())
}

fn model_tier(model: &str) -> Option<(&'static str, u8)> {
    let model = model.to_ascii_lowercase();
    if model.contains("claude") {
        for (name, tier) in [("haiku", 0), ("sonnet", 1), ("opus", 2)] {
            if model.contains(name) {
                return Some(("claude", tier));
            }
        }
    }
    if model.contains("gemini") {
        for (name, tier) in [("flash-lite", 0), ("flash", 1), ("pro", 2)] {
            if model.contains(name) {
                return Some(("gemini", tier));
            }
        }
    }
    None
}

fn prompt_variant(lead: &str, side: &str) -> &'static str {
    match (model_tier(lead), model_tier(side)) {
        (Some((_, l)), Some((_, s))) if l > s => "budget-worker-v1",
        (Some((_, l)), Some((_, s))) if l < s => "expert-worker-v1",
        _ => "default-v1",
    }
}

fn pairing_note(variant: &str) -> &'static str {
    match variant {
        "budget-worker-v1" => {
            "Pairing policy: give the worker a narrow implementation slice, settled interfaces, explicit file paths and a decisive verification command. Return evidence and blockers, not a new design. The lead adjudicates all ambiguous results."
        }
        "expert-worker-v1" => {
            "Pairing policy: use the worker's stronger analysis for bounded implementation and verification. State acceptance criteria and constraints without dictating unverified internals. The worker must expose assumptions and reproducible evidence; authority stays with the lead."
        }
        _ => "",
    }
}

fn next_tier(qualified: &str) -> Option<String> {
    let (token, model) = qualified.split_once(':')?;
    let (family, tier) = model_tier(model)?;
    let mut candidates = Vec::new();
    for home in [gray_home(), user_home()] {
        let text = std::fs::read_to_string(home.join("provider_models.json")).ok();
        let map = text.and_then(|t| serde_json::from_str::<BTreeMap<String, Value>>(&t).ok());
        for (key, value) in map.unwrap_or_default() {
            if provider_token(&key) != token {
                continue;
            }
            let rows = value.get("rows").unwrap_or(&value);
            for row in rows.as_array().into_iter().flatten() {
                if row.get("slots").is_some() {
                    continue;
                }
                let id = row.get("id").or_else(|| row.get(0)).and_then(Value::as_str);
                if let Some(id) = id
                    && model_tier(id) == Some((family, tier + 1))
                {
                    candidates.push(format!("{token}:{id}"));
                }
            }
        }
    }
    candidates.sort();
    candidates.pop()
}

#[derive(serde::Serialize, Clone, Copy)]
struct BriefSignals {
    has_artifact: bool,
    has_verification: bool,
}

fn brief_signals(task: &str) -> BriefSignals {
    let lower = task.to_ascii_lowercase();
    let has_verification = [
        "cargo check",
        "cargo test",
        "cargo clippy",
        "npm test",
        "npm run",
        "pnpm test",
        "pytest",
        "go test",
        "make check",
        "bash e2e/",
        "tsc --",
        "curl ",
    ]
    .iter()
    .any(|command| lower.contains(command));
    let has_artifact = has_verification
        || task.contains("```")
        || task.split_whitespace().any(|word| {
            word.contains('/')
                || [".rs", ".py", ".ts", ".tsx", ".json", ".sh", ".md", ".txt"]
                    .iter()
                    .any(|ext| word.contains(ext))
        });
    BriefSignals {
        has_artifact,
        has_verification,
    }
}

fn cold_notice(idle: Option<u64>) -> String {
    match idle {
        Some(seconds) if seconds > 300 => format!(
            "cold resume: conversation idle for {seconds}s; provider cache retention is unknown. Reuse saved conclusions; probe processes before restarting them.\n\n"
        ),
        _ => String::new(),
    }
}

#[derive(serde::Serialize)]
struct Attempt {
    rung: &'static str,
    model: String,
    prompt_variant: &'static str,
    outcome: &'static str,
    wall_secs: f64,
    usage: Value,
    requests: u64,
    tools: u64,
}

struct Ledger<'a> {
    handoff_id: &'a str,
    lead_model: &'a str,
    prompt_variant: &'a str,
    final_sidekick: &'a str,
    escalation_rung: &'a str,
    resolution: &'a str,
    attempts: &'a [Attempt],
    brief_signals: BriefSignals,
    lead: &'a str,
    qualified: &'a str,
    effort: Option<String>,
    resumed: bool,
    retried_fresh: bool,
    idle_secs: Option<u64>,
    outcome: &'a str,
    wall_secs: f64,
    brief_chars: usize,
    report_chars: usize,
    usage: Usage,
    requests: u64,
    tools: u64,
}

fn write_ledger(home: &Path, row: &Ledger) {
    let dir = home.join("fusion");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("stats.jsonl"))
    else {
        return;
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = json!({
        "ts": ts,
        "handoff_id": row.handoff_id,
        "lead_model": row.lead_model,
        "prompt_variant": row.prompt_variant,
        "final_sidekick": row.final_sidekick,
        "escalation_rung": row.escalation_rung,
        "resolution": row.resolution,
        "attempts": row.attempts,
        "brief_signals": row.brief_signals,
        "lead_sid": row.lead,
        "sidekick": row.qualified,
        "effort": row.effort,
        "resumed": row.resumed,
        "retried_fresh": row.retried_fresh,
        "idle_secs": row.idle_secs,
        "outcome": row.outcome,
        "wall_secs": (row.wall_secs * 1000.0).round() / 1000.0,
        "brief_chars": row.brief_chars,
        "report_chars": row.report_chars,
        "usage": row.usage.to_json(),
        "requests": row.requests,
        "tools": row.tools,
    })
    .to_string();
    let _ = f.write_all(format!("{line}\n").as_bytes());
}

fn row_tokens(row: &Value) -> u64 {
    ["input_tokens", "output_tokens"]
        .iter()
        .map(|key| {
            row.get("usage")
                .and_then(|u| u.get(*key))
                .and_then(Value::as_u64)
                .unwrap_or(0)
        })
        .sum()
}

fn cohort(rows: &[&Value]) -> String {
    let ok = rows
        .iter()
        .filter(|r| r.get("outcome").and_then(Value::as_str) == Some("ok"))
        .count();
    let tokens: u64 = rows.iter().map(|r| row_tokens(r)).sum();
    let per_completed = if ok == 0 {
        "n/a".into()
    } else {
        format!("{:.0}", tokens as f64 / ok as f64)
    };
    let wall: f64 = rows
        .iter()
        .map(|r| r.get("wall_secs").and_then(Value::as_f64).unwrap_or(0.0))
        .sum();
    format!(
        "n={} ok={ok} tokens={tokens} tokens/completed={per_completed} mean_wall={:.1}s",
        rows.len(),
        wall / rows.len().max(1) as f64
    )
}

fn fusion_stats(argv: &[String], sid: &str) -> String {
    let all = argv.iter().any(|a| a == "--all") || sid.is_empty();
    let path = gray_home().join("fusion/stats.jsonl");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let rows: Vec<Value> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| {
            v.is_object() && (all || v.get("lead_sid").and_then(Value::as_str) == Some(sid))
        })
        .collect();
    let scope = if all {
        "all sessions".to_string()
    } else {
        format!("session {sid}")
    };
    if argv.first().is_some_and(|a| a == "compare") {
        let variants: Vec<&str> = argv
            .iter()
            .skip(1)
            .filter(|a| !a.starts_with("--"))
            .map(String::as_str)
            .collect();
        if variants.len() != 2 {
            return "usage: fusion compare <variant-a> <variant-b> [--all]\nCompares recorded handoffs, not benchmark runs or dollar costs.".into();
        }
        let mut out = format!(
            "fusion compare: {scope}\nvariant | recorded handoffs (all attempted work included in tokens/completed)\n"
        );
        for variant in variants {
            let selected: Vec<&Value> = rows
                .iter()
                .filter(|r| r.get("prompt_variant").and_then(Value::as_str) == Some(variant))
                .collect();
            out.push_str(&format!("{variant} | {}\n", cohort(&selected)));
        }
        out.push_str("Observational token proxy, not dollar savings or a causal benchmark. Pairings and task difficulty may differ; zero completed handoffs yields n/a.");
        return out;
    }
    if rows.is_empty() {
        return format!("fusion stats: no handoffs recorded ({scope})");
    }
    let mut outcomes = BTreeMap::<String, usize>::new();
    let mut rungs = BTreeMap::<String, usize>::new();
    let mut attempt_rungs = BTreeMap::<String, usize>::new();
    let mut groups = BTreeMap::<(String, String, String), Vec<&Value>>::new();
    let mut briefs = BTreeMap::<String, Vec<&Value>>::new();
    let mut tokens = [0u64; 5];
    let mut cold = 0;
    let mut wall = 0.0;
    for row in &rows {
        *outcomes.entry(str_field(row, "outcome")).or_default() += 1;
        let rung = row
            .get("escalation_rung")
            .and_then(Value::as_str)
            .unwrap_or("legacy");
        *rungs.entry(rung.into()).or_default() += 1;
        for attempt in row
            .get("attempts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            *attempt_rungs.entry(str_field(attempt, "rung")).or_default() += 1;
        }
        let variant = row
            .get("prompt_variant")
            .and_then(Value::as_str)
            .unwrap_or("legacy-unattributed");
        groups
            .entry((
                str_field(row, "lead_model"),
                str_field(row, "sidekick"),
                variant.into(),
            ))
            .or_default()
            .push(row);
        let signal = row.get("brief_signals");
        let bucket = match signal {
            None => "unknown",
            Some(s)
                if s.get("has_artifact").and_then(Value::as_bool) == Some(true)
                    && s.get("has_verification").and_then(Value::as_bool) == Some(true) =>
            {
                "structured"
            }
            Some(s) if s.get("has_artifact").and_then(Value::as_bool) == Some(true) => {
                "artifact-only"
            }
            _ => "unstructured",
        };
        briefs.entry(bucket.into()).or_default().push(row);
        cold += usize::from(
            row.get("idle_secs")
                .and_then(Value::as_u64)
                .is_some_and(|i| i > 300),
        );
        wall += row.get("wall_secs").and_then(Value::as_f64).unwrap_or(0.0);
        for (i, key) in [
            "input_tokens",
            "output_tokens",
            "cache_read_input_tokens",
            "cache_write_input_tokens",
            "reasoning_tokens",
        ]
        .iter()
        .enumerate()
        {
            tokens[i] += row
                .get("usage")
                .and_then(|u| u.get(*key))
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
    }
    let count = |key: &str| outcomes.get(key).copied().unwrap_or(0);
    let mut out = format!(
        "fusion stats: {scope}\nhandoffs: {}\noutcomes: ok={} failed={} timeout={} cancelled={}\nsidekick tokens: input={} output={} cache_read={} cache_write={} reasoning={}\ncold resumes (idle > 300s): {cold}\nmean wall: {:.1}s\nescalation rungs: {}\n",
        rows.len(),
        count("ok"),
        count("failed"),
        count("timeout"),
        count("cancelled"),
        tokens[0],
        tokens[1],
        tokens[2],
        tokens[3],
        tokens[4],
        wall / rows.len() as f64,
        rungs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    out.push_str(&format!(
        "executed attempt rungs: {}\n",
        attempt_rungs
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ")
    ));
    for ((lead, side, variant), selected) in groups {
        out.push_str(&format!(
            "pairing {lead} -> {side} | {variant}: {}\n",
            cohort(&selected)
        ));
    }
    for (bucket, selected) in briefs {
        let failed = selected
            .iter()
            .filter(|r| {
                matches!(
                    r.get("outcome").and_then(Value::as_str),
                    Some("failed" | "timeout")
                )
            })
            .count();
        out.push_str(&format!(
            "brief {bucket}: n={} failed_or_timeout={failed} rate={:.1}%\n",
            selected.len(),
            100.0 * failed as f64 / selected.len() as f64
        ));
    }
    out.push_str(
        "Brief signals are lexical observations, not a quality score or evidence of causation.",
    );
    out
}

fn handoff_timeout() -> Duration {
    std::env::var("GRAY_FUSION_HANDOFF_TIMEOUT")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
}

fn gray_bin() -> String {
    std::env::var("GRAY_BIN")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "gray".to_string())
}

fn rand_id() -> String {
    let mut buf = [0u8; 8];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    } else {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        buf = (now.as_nanos() as u64 ^ std::process::id() as u64).to_le_bytes();
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

struct SlotState {
    finished: bool,
    reply: Option<Value>,
}

struct Slot {
    handoff_id: String,
    state: Mutex<SlotState>,
    done: Condvar,
    cancel: AtomicBool,
    pgid: AtomicU32,
}

fn slots() -> std::sync::MutexGuard<'static, HashMap<String, Arc<Slot>>> {
    static SLOTS: OnceLock<Mutex<HashMap<String, Arc<Slot>>>> = OnceLock::new();
    SLOTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn last_reports() -> std::sync::MutexGuard<'static, HashMap<String, String>> {
    static REPORTS: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    REPORTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Pump {
    completed: bool,
    session_id: Option<String>,
    text: String,
    error: Option<String>,
    killed: bool,
    usage: Usage,
    requests: u64,
    tools: u64,
    last_text: String,
    last_tools: Vec<String>,
}

#[derive(Default, Clone, Copy)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_input_tokens: u64,
    cache_write_input_tokens: u64,
    reasoning_tokens: u64,
}

impl Usage {
    fn add_json(&mut self, v: &Value) {
        let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        self.input_tokens += g("input_tokens");
        self.output_tokens += g("output_tokens");
        self.cache_read_input_tokens += g("cache_read_input_tokens");
        self.cache_write_input_tokens += g("cache_write_input_tokens");
        self.reasoning_tokens += g("reasoning_tokens");
    }

    fn set_json(&mut self, v: &Value) {
        let g = |k: &str| v.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
        self.input_tokens = g("input_tokens");
        self.output_tokens = g("output_tokens");
        self.cache_read_input_tokens = g("cache_read_input_tokens");
        self.cache_write_input_tokens = g("cache_write_input_tokens");
        self.reasoning_tokens = g("reasoning_tokens");
    }

    fn to_json(self) -> Value {
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_input_tokens,
            "cache_write_input_tokens": self.cache_write_input_tokens,
            "reasoning_tokens": self.reasoning_tokens,
        })
    }
}

fn handle_row(line: &str, pump: &mut Pump, home: &Path, lead: &str, qualified: &str) {
    let Ok(row) = serde_json::from_str::<Value>(line.trim()) else {
        return;
    };
    if let Some(sid) = row.get("session_id").and_then(|v| v.as_str())
        && !sid.is_empty()
        && sid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && pump.session_id.as_deref() != Some(sid)
    {
        pump.session_id = Some(sid.to_string());
        write_session(home, lead, qualified, sid);
    }
    match row.get("type").and_then(|v| v.as_str()) {
        Some("result") => {
            pump.completed = true;
            if let Some(t) = row.get("text").and_then(|v| v.as_str()) {
                pump.text = t.to_string();
            }
            if let Some(u) = row.get("usage") {
                pump.usage.set_json(u);
            }
            if let Some(n) = row.pointer("/accounting/requests").and_then(|v| v.as_u64()) {
                pump.requests = pump.requests.max(n);
            }
        }
        Some("error") => {
            let msg = row
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown sidekick error");
            pump.error = Some(match row.get("code").and_then(|v| v.as_str()) {
                Some(code) => format!("{code}: {msg}"),
                None => msg.to_string(),
            });
        }
        Some("progress") => {
            match row.get("phase").and_then(|v| v.as_str()) {
                Some("request") => pump.requests += 1,
                Some("text") => {
                    if let Some(t) = row.get("text").and_then(|v| v.as_str()) {
                        pump.last_text.push_str(t);
                        if pump.last_text.len() > 4096 {
                            let cut = pump.last_text.len() - 4096;
                            let cut = pump.last_text.floor_char_boundary(cut);
                            pump.last_text.drain(..cut);
                        }
                    }
                }
                Some("tool_started" | "tool_ran" | "tool_finished") => {
                    if row.get("phase").and_then(|p| p.as_str()) == Some("tool_started") {
                        pump.tools += 1;
                    }
                    let label = row
                        .get("label")
                        .or_else(|| row.get("tool"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("tool");
                    let mut note = label.to_string();
                    if let Some(d) = row.get("detail").and_then(|v| v.as_str()) {
                        note = format!("{note} {d}");
                    }
                    if let Some(o) = row.get("output").and_then(|v| v.as_str()) {
                        let head: String = o.chars().take(120).collect();
                        note = format!("{note} -> {head}");
                    }
                    pump.last_tools.push(note);
                    if pump.last_tools.len() > 8 {
                        let drop = pump.last_tools.len() - 8;
                        pump.last_tools.drain(..drop);
                    }
                }
                _ => {}
            }
            if let Some(u) = row.get("usage") {
                pump.usage.add_json(u);
            }
        }
        _ => {}
    }
}

/// Sweep stray processes still tagged with this handoff's env marker.
/// Linux-only (`/proc/<pid>/environ`); a no-op elsewhere — `kill_tree`'s
/// group/taskkill path is the portable cleanup.
#[cfg(target_os = "linux")]
fn sweep_tagged(handoff_id: &str) {
    let needle = format!("{ENV_HANDOFF_ID}={handoff_id}");
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Ok(pid) = name.parse::<i32>() else {
            continue;
        };
        if pid == std::process::id() as i32 || pid <= 1 {
            continue;
        }
        let Ok(bytes) = std::fs::read(entry.path().join("environ")) else {
            continue;
        };
        if bytes.split(|b| *b == 0).any(|v| v == needle.as_bytes()) {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn sweep_tagged(_handoff_id: &str) {}

/// SIGTERM the child's whole process group; on Windows taskkill the tree.
fn terminate_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T"])
        .output();
}

/// SIGKILL the process group after the grace window; taskkill /F on Windows.
fn force_kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(windows)]
    let _ = Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/T", "/F"])
        .output();
}

fn kill_tree(slot: &Slot, child: &mut Child, pid: u32) {
    if pid != 0 {
        terminate_tree(pid);
        let deadline = Instant::now() + TERM_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                _ => break,
            }
        }
        force_kill_tree(pid);
    }
    let deadline = Instant::now() + REAP_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    sweep_tagged(&slot.handoff_id);
}

struct RunCtx<'a> {
    cwd: &'a Path,
    home: &'a Path,
    lead: &'a str,
    qualified: &'a str,
    effort: Option<&'a str>,
    variant: &'static str,
}

fn run_child(
    slot: &Arc<Slot>,
    spec: &SidekickSpec,
    task: &str,
    session: Option<&str>,
    ctx: &RunCtx,
) -> Pump {
    let cwd = ctx.cwd;
    let home = ctx.home;
    let lead = ctx.lead;
    let qualified = ctx.qualified;
    let mut pump = Pump::default();
    if slot.cancel.load(Ordering::SeqCst) {
        pump.killed = true;
        pump.error = Some("handoff cancelled".into());
        return pump;
    }
    let timeout = handoff_timeout();
    let task_owned;
    let task = if session.is_none() {
        task_owned = format!(
            "{SIDEKICK_CONTRACT}\n{}\n{task}\n</lead_handoff>",
            pairing_note(ctx.variant)
        );
        task_owned.as_str()
    } else {
        task
    };
    let mut cmd = Command::new(gray_bin());
    cmd.arg("-p")
        .arg(task)
        .arg("--json")
        .arg("--model")
        .arg(&spec.model)
        .arg("--max-requests")
        .arg("200");
    if let Some(sid) = session {
        cmd.arg("--session").arg(sid);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.env("GRAY_HOME", home)
        .env(ENV_SIDEKICK, "1")
        .env("GRAY_SHOW_REASONING", "0")
        .env(ENV_HANDOFF_ID, &slot.handoff_id)
        .env(
            "GRAY_MAX_WALL_SECS",
            timeout
                .as_secs()
                .saturating_sub(60)
                .max(timeout.as_secs() / 2)
                .to_string(),
        );
    if let Some(effort) = ctx.effort {
        cmd.env("GRAY_THINKING_EFFORT", effort);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    for k in &spec.env_remove {
        cmd.env_remove(k);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            pump.error = Some(format!("sidekick spawn failed: {e}"));
            return pump;
        }
    };
    let pid = child.id();
    slot.pgid.store(pid, Ordering::SeqCst);
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        pump.error = Some("sidekick spawn failed: no pipes".into());
        kill_tree(slot, &mut child, pid);
        return pump;
    };
    let stderr_task = std::thread::spawn(move || {
        let mut tail: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&chunk[..n]);
                    if tail.len() > STDERR_TAIL {
                        tail.drain(..tail.len() - STDERR_TAIL);
                    }
                }
            }
        }
        tail
    });
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let reader = std::thread::spawn(move || {
        let rd = BufReader::new(stdout);
        for line in rd.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + timeout;
    enum Done {
        Eof,
        Killed(&'static str),
    }
    let done = loop {
        if slot.cancel.load(Ordering::SeqCst) {
            break Done::Killed("handoff cancelled");
        }
        if Instant::now() >= deadline {
            break Done::Killed("handoff timed out");
        }
        if let Ok(Some(_)) = child.try_wait() {
            force_kill_tree(pid);
            sweep_tagged(&slot.handoff_id);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let line = if line.len() > LINE_CAP {
                    &line[..line.floor_char_boundary(LINE_CAP)]
                } else {
                    &line
                };
                handle_row(line, &mut pump, home, lead, qualified);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break Done::Killed("handoff timed out");
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                while let Ok(line) = rx.try_recv() {
                    handle_row(&line, &mut pump, home, lead, qualified);
                }
                break Done::Eof;
            }
        }
    };
    if let Done::Killed(msg) = done {
        kill_tree(slot, &mut child, pid);
        pump.killed = true;
        pump.error = Some(msg.into());
        let _ = reader.join();
        let _ = stderr_task.join();
        slot.pgid.store(0, Ordering::SeqCst);
        return pump;
    }
    let deadline2 = Instant::now() + REAP_GRACE;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline2 => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                kill_tree(slot, &mut child, pid);
                break None;
            }
            Err(_) => break None,
        }
    };
    force_kill_tree(pid);
    sweep_tagged(&slot.handoff_id);
    slot.pgid.store(0, Ordering::SeqCst);
    let _ = reader.join();
    let stderr_tail = stderr_task.join().unwrap_or_default();
    match status {
        Some(s) if !s.success() && pump.error.is_none() => {
            let tail = String::from_utf8_lossy(&stderr_tail);
            let tail = tail.trim();
            let code = s
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| s.to_string());
            pump.error = Some(if tail.is_empty() {
                format!("sidekick exited with status {code}")
            } else {
                format!("sidekick exited with status {code}: {tail}")
            });
        }
        None if pump.error.is_none() => {
            pump.error = Some("sidekick did not exit".into());
        }
        _ => {}
    }
    if !pump.completed && pump.error.is_none() {
        pump.error = Some("sidekick exited without a result row".into());
    }
    pump
}

fn partial_report(pump: &Pump) -> String {
    let mut parts = Vec::new();
    if !pump.last_text.trim().is_empty() {
        let tail: String = pump
            .last_text
            .chars()
            .rev()
            .take(600)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        parts.push(format!("last sidekick output:\n{}", tail.trim()));
    }
    if !pump.last_tools.is_empty() {
        parts.push(format!(
            "recent tool activity:\n{}",
            pump.last_tools.join("\n")
        ));
    }
    parts.join("\n\n")
}

fn pump_handoff(slot: &Arc<Slot>, task: &str, cwd: &Path, lead: &str, active: &Active) -> Value {
    let started = Instant::now();
    let home = gray_home();
    let effort = sidekick_effort();
    let qualified = &active.sidekick;
    let resumed = read_session(&home, lead, qualified).is_some();
    let idle_secs = resumed
        .then(|| session_idle_secs(&home, lead, qualified))
        .flatten();
    let variant = prompt_variant(&active.lead_model, qualified);
    let mut model = qualified.clone();
    let mut brief = task.to_string();
    let mut evidence = Vec::new();
    let mut attempts = Vec::new();
    let mut total = Usage::default();
    let mut requests = 0;
    let mut tools = 0;
    let mut retried_fresh = false;
    let mut rung = "initial";
    let mut outcome = "failed";
    let mut report = String::new();
    let mut terminal_reason = String::new();
    for index in 0..3 {
        let attempt_start = Instant::now();
        let session = if index == 2 {
            None
        } else {
            read_session(&home, lead, &model)
        };
        let attempt_variant = prompt_variant(&active.lead_model, &model);
        let spec = resolve_sidekick(&model);
        let pump = match &spec {
            Some(spec) => run_child(
                slot,
                spec,
                &brief,
                session.as_deref(),
                &RunCtx {
                    cwd,
                    home: &home,
                    lead,
                    qualified: &model,
                    effort: effort.as_deref(),
                    variant: attempt_variant,
                },
            ),
            None => Pump {
                error: Some(format!("cannot resolve sidekick model '{model}'")),
                ..Pump::default()
            },
        };
        outcome = if slot.cancel.load(Ordering::SeqCst)
            || pump.error.as_deref() == Some("handoff cancelled")
        {
            "cancelled"
        } else if pump.killed {
            "timeout"
        } else if pump.error.is_some() {
            "failed"
        } else {
            "ok"
        };
        total.add_json(&pump.usage.to_json());
        requests += pump.requests;
        tools += pump.tools;
        attempts.push(Attempt {
            rung,
            model: model.clone(),
            prompt_variant: attempt_variant,
            outcome,
            wall_secs: attempt_start.elapsed().as_secs_f64(),
            usage: pump.usage.to_json(),
            requests: pump.requests,
            tools: pump.tools,
        });
        if let Some(sid) = &pump.session_id {
            write_session(&home, lead, &model, sid);
        }
        if outcome == "ok" {
            report = pump.text;
            if index > 0 {
                report.push_str(&format!(
                    "\n\nRecovered on {rung}. Prior failure evidence:\n{}",
                    evidence.join("\n\n")
                ));
            }
            break;
        }
        let detail = format!(
            "{rung}: {outcome}: {}\n{}",
            pump.error.as_deref().unwrap_or("handoff cancelled"),
            partial_report(&pump)
        );
        evidence.push(detail.chars().take(3000).collect::<String>());
        if outcome == "cancelled" {
            let retention = if read_session(&home, lead, &model).is_some() {
                "session kept and will resume"
            } else {
                "no saved session is available"
            };
            report = format!(
                "handoff cancelled; {retention}\n\n{}",
                evidence.join("\n\n")
            );
            break;
        }
        if spec.is_none() {
            terminal_reason = "provider/model resolution is unavailable".into();
            break;
        }
        if index == 0 {
            if !pump.killed && resumed {
                drop_session(&home, lead, &model);
                retried_fresh = true;
            }
            rung = "repair";
        } else if index == 1 {
            if let Some(next) = next_tier(&model) {
                model = next;
                rung = "tier";
            } else {
                terminal_reason = "no next model tier is advertised by this provider; no model name or credential was invented".into();
                break;
            }
        }
        brief = format!(
            "{task}\n\n<failure_evidence>\n{}\n</failure_evidence>\nCorrect the evidenced failure, inspect artifacts before repeating work, and run the brief's verification. Do not repeat an unchanged failing approach. Report a blocker if the lead's decision is required.",
            evidence.join("\n\n")
        );
    }
    let resolution = if outcome == "ok" {
        "complete"
    } else if outcome == "cancelled" {
        "cancelled"
    } else {
        "needs_lead"
    };
    if resolution == "needs_lead" {
        rung = "needs_lead";
        report = format!(
            "this needs your hands: handoff {outcome} after {} attempt(s). {terminal_reason}\n\n{}",
            attempts.len(),
            evidence.join("\n\n")
        );
    }
    report = format!("{}{report}", cold_notice(idle_secs));
    let reply = json!({"content": report, "is_error": outcome != "ok", "usage": total.to_json()});
    write_ledger(
        &home,
        &Ledger {
            handoff_id: &slot.handoff_id,
            lead,
            lead_model: &active.lead_model,
            qualified,
            prompt_variant: variant,
            final_sidekick: &model,
            escalation_rung: rung,
            resolution,
            attempts: &attempts,
            brief_signals: brief_signals(task),
            effort,
            resumed,
            retried_fresh,
            idle_secs,
            outcome,
            wall_secs: started.elapsed().as_secs_f64(),
            brief_chars: task.chars().count(),
            report_chars: report.chars().count(),
            usage: total,
            requests,
            tools,
        },
    );
    let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
    st.finished = true;
    st.reply = Some(reply.clone());
    drop(st);
    slot.done.notify_all();
    reply
}

fn remove_slot(lead: &str, slot: &Arc<Slot>) {
    let mut map = slots();
    if map
        .get(lead)
        .is_some_and(|current| Arc::ptr_eq(current, slot))
    {
        map.remove(lead);
    }
}

fn tool_call(params: &Value) -> Value {
    let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let args = params.get("args").cloned().unwrap_or(Value::Null);
    let session = params.get("session").cloned().unwrap_or(Value::Null);
    let cwd = session
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let lead = session
        .get("id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("nosession")
        .to_string();
    let Some(active) = active(Some(&session)) else {
        return json!({
            "content": "gray fusion inactive: select the gray-fusion model row",
            "is_error": true,
        });
    };
    match name {
        "handoff" => {
            let task = args
                .get("task")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if task.trim().is_empty() {
                return json!({"content": "handoff requires a task", "is_error": true});
            }
            let block = args.get("block").and_then(|v| v.as_bool()).unwrap_or(true);
            let slot = Arc::new(Slot {
                handoff_id: rand_id(),
                state: Mutex::new(SlotState {
                    finished: false,
                    reply: None,
                }),
                done: Condvar::new(),
                cancel: AtomicBool::new(false),
                pgid: AtomicU32::new(0),
            });
            {
                let mut map = slots();
                if let Some(prev) = map.get(&lead) {
                    let st = prev.state.lock().unwrap_or_else(|e| e.into_inner());
                    return json!({
                        "content": if st.finished { "sidekick report ready; call handoff_wait to collect it" } else { "sidekick busy; call handoff_wait" },
                        "is_error": true,
                    });
                }
                map.insert(lead.clone(), slot.clone());
            }
            let notice = cold_notice(session_idle_secs(&gray_home(), &lead, &active.sidekick));
            let pump_slot = slot.clone();
            let cwd2 = cwd.clone();
            let lead2 = lead.clone();
            std::thread::spawn(move || {
                pump_handoff(&pump_slot, &task, &cwd2, &lead2, &active);
            });
            if !block {
                return json!({
                    "content": format!("{notice}handoff {} running in background; call handoff_wait to collect the report", slot.handoff_id),
                    "is_error": false,
                });
            }
            let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            while !st.finished {
                st = slot.done.wait(st).unwrap_or_else(|e| e.into_inner());
            }
            let reply = st
                .reply
                .take()
                .unwrap_or(json!({"content":"", "is_error":false}));
            drop(st);
            remove_slot(&lead, &slot);
            if let Some(c) = reply.get("content").and_then(|v| v.as_str()) {
                last_reports().insert(lead.clone(), c.to_string());
            }
            reply
        }
        "handoff_wait" => {
            let seconds = args
                .get("seconds")
                .and_then(|v| v.as_u64())
                .unwrap_or(WAIT_DEFAULT_SECS)
                .min(WAIT_MAX_SECS);
            let slot = slots().get(&lead).cloned();
            let Some(slot) = slot else {
                let last = last_reports().get(&lead).cloned();
                let content = match last {
                    Some(r) => format!("no handoff running; last report:\n{r}"),
                    None => "no handoff running".to_string(),
                };
                return json!({"content": content, "is_error": false});
            };
            let deadline = Instant::now() + Duration::from_secs(seconds.max(1));
            let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
            while !st.finished {
                let now = Instant::now();
                if now >= deadline {
                    return json!({
                        "content": format!("handoff still running after {seconds}s"),
                        "is_error": false,
                    });
                }
                let wait = deadline - now;
                let (guard, _t) = slot
                    .done
                    .wait_timeout(st, wait)
                    .unwrap_or_else(|e| e.into_inner());
                st = guard;
            }
            let reply = st
                .reply
                .take()
                .unwrap_or(json!({"content":"", "is_error":false}));
            drop(st);
            remove_slot(&lead, &slot);
            if let Some(c) = reply.get("content").and_then(|v| v.as_str()) {
                last_reports().insert(lead.clone(), c.to_string());
            }
            reply
        }
        _ => json!({
            "content": format!("unknown tool {name}"),
            "is_error": true,
        }),
    }
}

fn tool_defs(active: &Active) -> Vec<Value> {
    let variant = prompt_variant(&active.lead_model, &active.sidekick);
    let prompt = format!(
        "{LEAD_PROMPT}\n\nPrompt variant: {variant}\n{}",
        pairing_note(variant)
    );
    vec![
        json!({
            "name": "handoff",
            "label": "handoff",
            "description": prompt,
            "parameters": {
                "type": "object",
                "properties": {
                    "task": {"type": "string", "description": "self-contained brief for the sidekick"},
                    "label": {"type": "string", "description": "short label for the handoff"},
                    "block": {"type": "boolean", "description": "wait for the sidekick report (default true); pass false for background", "default": true}
                },
                "required": ["task"]
            },
        }),
        json!({
            "name": "handoff_wait",
            "label": "handoff wait",
            "description": "Collect the report of a background `handoff` started with block:false. Waits up to `seconds` (default 300, max 3600); returns the sidekick's report once, then answers 'no handoff running' plus the last report.",
            "parameters": {
                "type": "object",
                "properties": {
                    "seconds": {"type": "integer", "description": "max seconds to wait (default 300, max 3600)", "default": 300}
                }
            },
        }),
    ]
}

fn manifest() -> Value {
    json!({
        "name": "fusion",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": "1.3",
        "tools": [],
        "commands": ["/fusion"],
        "command_descriptions": {"/fusion": "sidekick ledger: stats [--all]; compare <variant-a> <variant-b> [--all]"},
        "composite_rows": [{
            "id": "gray-fusion",
            "name": "Fusion",
            "slots": [
                {"key": "lead", "label": "Lead", "source": "own"},
                {"key": "sidekick", "label": "Sidekick", "source": "all"}
            ]
        }]
    })
}

fn cancel_lead(lead: &str) {
    if let Some(slot) = slots().get(lead) {
        slot.cancel.store(true, Ordering::SeqCst);
    }
}

fn shutdown_drain() {
    let leads: Vec<String> = slots().keys().cloned().collect();
    for lead in &leads {
        cancel_lead(lead);
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let all_done = {
            let map = slots();
            map.values()
                .all(|s| s.state.lock().unwrap_or_else(|e| e.into_inner()).finished)
        };
        if all_done || Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn reply(id: u64, result: Value) {
    static STDOUT: OnceLock<Mutex<std::io::Stdout>> = OnceLock::new();
    let out = STDOUT.get_or_init(|| Mutex::new(std::io::stdout()));
    let mut g = out.lock().unwrap_or_else(|e| e.into_inner());
    let _ = writeln!(g, "{}", json!({"id": id, "result": result}));
    let _ = g.flush();
}

fn main() -> anyhow::Result<()> {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("manifest") {
        println!("{}", manifest());
        return Ok(());
    }
    if matches!(arg.as_deref(), Some("stats" | "compare")) {
        println!(
            "{}",
            fusion_stats(&std::env::args().skip(1).collect::<Vec<_>>(), "")
        );
        return Ok(());
    }
    if matches!(arg.as_deref(), Some("--help" | "-h")) {
        println!(
            "gray-fusion: lead + conversation-persistent sidekick\nCommands: manifest; stats [--all]; compare <variant-a> <variant-b> [--all]\nTools: handoff, handoff_wait. Busy calls are rejected, not steering.\nFailure ladder: initial -> evidence repair -> one advertised tier -> lead.\nTimeout applies per attempt (at most three); cancellation never retries.\nChild processes are not persistent. Cold means idle >300s, not a measured cache miss."
        );
        return Ok(());
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if method == "plugin/shutdown" {
            break;
        }
        if method == "tool/cancel" {
            let lead = req
                .pointer("/params/session/id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or("nosession");
            cancel_lead(lead);
            continue;
        }
        let Some(id) = req.get("id").and_then(|i| i.as_u64()) else {
            continue;
        };
        match req.get("method").and_then(|m| m.as_str()) {
            Some("plugin/manifest") => reply(id, manifest()),
            Some("plugin/tools") => {
                let sess = req
                    .get("params")
                    .and_then(|p| p.get("session"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let tools = active(Some(&sess))
                    .map(|a| tool_defs(&a))
                    .unwrap_or_default();
                reply(id, json!({"tools": tools}));
            }
            Some("command/run") => {
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                let name = params
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim_start_matches('/')
                    .to_string();
                let argv: Vec<String> = params
                    .get("argv")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let sid = params
                    .pointer("/session/id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let text = if name == "fusion" {
                    fusion_stats(&argv, &sid)
                } else {
                    format!("unknown command {name}")
                };
                reply(id, json!({"text": text}));
            }
            Some("tool/call") => {
                let params = req.get("params").cloned().unwrap_or(Value::Null);
                std::thread::spawn(move || {
                    let result = tool_call(&params);
                    reply(id, result);
                });
            }
            Some(method) => reply(id, json!({"error": format!("no handler for {method}")})),
            None => {}
        }
    }
    shutdown_drain();
    Ok(())
}
