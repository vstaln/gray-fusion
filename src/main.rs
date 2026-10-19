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

const DEFAULT_TIMEOUT_SECS: u64 = 1800;
const WAIT_DEFAULT_SECS: u64 = 300;
const WAIT_MAX_SECS: u64 = 3600;
const TERM_GRACE: Duration = Duration::from_secs(10);
const REAP_GRACE: Duration = Duration::from_secs(5);
const LINE_CAP: usize = 1024 * 1024;
const STDERR_TAIL: usize = 2048;

const LEAD_PROMPT: &str = r#"\
You have a `handoff` tool: a persistent sidekick subagent that works alongside you on the same machine (shared filesystem and repos; its shell sessions are separate from yours). You are the lead: you own the outcome and the user-facing and authority actions — talking to the user, planning, and directing the sidekick. The user interacts with one agent: you. Unless they explicitly ask about the sidekick, do not mention it or distinguish its work from yours; own the combined result and present it in the first person.

- **Delegate by default** the hands-on work: implementation, verification (builds, lints, type-checks, test runs), environment setup and repair, and broad fan-out searches where you only need the conclusion. Keep for yourself: planning and design, judgment calls, correctness-critical work where wrong output looks plausible (queries, measurements, eval harnesses, pipeline/model configuration — author and check those yourself), reviewing the sidekick's diffs, and everything user-facing or authority-bound (commits, pushes, PRs, answering the user).
- A `handoff` call blocks until the sidekick reports back by default; pass `"block": false` to start it in the background and collect the report later with `handoff_wait` (use it for work that can run while you do something else). Never run the same work yourself in parallel or poll for its result. Write each brief self-contained: the goal, the context and file pointers it needs, your plan, the constraints, and the narrowest verification commands that cover the change — the sidekick treats your list as mandatory, so a "run everything" brief re-gates unchanged work on every handoff.
- It remembers previous handoffs and its runtime state persists (dev servers, background jobs, in-flight commands) — don't re-explain what it already knows. For work involving servers or long-running processes, say what may still be running and whether to leave it up.
- Never make the sidekick redo work you already did: results you derived go into the brief as settled inputs. And review the evidence it reports back (diffs, logs, test output, artifact paths) instead of re-running its verification — check yourself only when the evidence is missing or suspicious."#;

const SIDEKICK_CONTRACT: &str = "<sidekick_contract>
You are the sidekick: a persistent collaborator agent executing task briefs handed off by the lead agent. You do not see the user's messages or the lead's conversation — everything you need is in the brief. Work autonomously with your own tools and verify your work before reporting.

- The lead owns user-facing and authority actions. Do not contact the user, create or update pull requests, commit, or push — for ordinary implementation tasks leave the work tree ready for the lead to review. If the task needs something only the lead or user can do (a secret, information only the user has, a PR action), stop and report what you need instead of working around it.
- Git safety: never run destructive or irreversible git commands unless the brief directs it; stage files explicitly (never `git add .`) and never stage files likely to contain secrets.
- Keep diffs minimal and match the repo's existing conventions; touch only what the task asks for.
- Report back so it stands alone — the user never sees your output: what you did or found, how you verified it (commands run, results, evidence paths), any deviations, and anything left for the lead.
</sidekick_contract>

Brief:
";

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

struct Active {
    sidekick: String,
}

fn active_from_model(m: &Value) -> Option<Active> {
    if m.get("id").and_then(|v| v.as_str()) != Some("gray-fusion") {
        return None;
    }
    let parts: BTreeMap<String, String> = m
        .get("parts")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let sidekick = parts.get("sidekick")?.clone();
    let qualified = if sidekick.contains(':') {
        sidekick
    } else {
        let pid = m.get("provider_id").and_then(|v| v.as_str()).unwrap_or("");
        let base = m.get("base_url").and_then(|v| v.as_str()).unwrap_or("");
        let list_key = if !pid.is_empty() {
            plugin_models_key(pid)
        } else {
            normalize_base_url(base)
        };
        format!("{}:{sidekick}", provider_token(&list_key))
    };
    Some(Active {
        sidekick: qualified,
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

struct Pump {
    session_id: Option<String>,
    text: String,
    error: Option<String>,
    killed: bool,
    usage: Usage,
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
            if let Some(t) = row.get("text").and_then(|v| v.as_str()) {
                pump.text = t.to_string();
            }
            if let Some(u) = row.get("usage") {
                pump.usage.set_json(u);
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
    let mut pump = Pump {
        session_id: None,
        text: String::new(),
        error: None,
        killed: false,
        usage: Usage::default(),
        last_text: String::new(),
        last_tools: Vec::new(),
    };
    let timeout = handoff_timeout();
    let task_owned;
    let task = if session.is_none() {
        task_owned = format!("{SIDEKICK_CONTRACT}\n{task}");
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
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let line = if line.len() > LINE_CAP {
                    &line[..LINE_CAP]
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
        return pump;
    }
    let _ = reader.join();
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

fn pump_handoff(slot: &Arc<Slot>, task: &str, cwd: &Path, lead: &str, qualified: &str) -> Value {
    let home = gray_home();
    let Some(spec) = resolve_sidekick(qualified) else {
        return json!({
            "content": format!("cannot resolve sidekick model '{qualified}'"),
            "is_error": true,
        });
    };
    let session = read_session(&home, lead, qualified);
    let resumed = session.is_some();
    let ctx = RunCtx {
        cwd,
        home: &home,
        lead,
        qualified,
    };
    let pump = run_child(slot, &spec, task, session.as_deref(), &ctx);
    let pump = if pump.error.is_some() && !pump.killed && resumed {
        drop_session(&home, lead, qualified);
        let mut retry = run_child(slot, &spec, task, None, &ctx);
        retry.usage.input_tokens += pump.usage.input_tokens;
        retry.usage.output_tokens += pump.usage.output_tokens;
        retry.usage.cache_read_input_tokens += pump.usage.cache_read_input_tokens;
        retry.usage.cache_write_input_tokens += pump.usage.cache_write_input_tokens;
        retry.usage.reasoning_tokens += pump.usage.reasoning_tokens;
        retry
    } else {
        pump
    };
    let timeout_secs = handoff_timeout().as_secs();
    let reply = if pump.killed {
        let sid = pump.session_id.clone().unwrap_or_else(|| "none".into());
        let cancelled = pump.error.as_deref() == Some("handoff cancelled");
        let mut content = if cancelled {
            format!("handoff cancelled; sidekick session {sid} kept and will resume")
        } else {
            format!(
                "handoff timed out after {timeout_secs}s; sidekick session {sid} kept and will resume"
            )
        };
        let partial = partial_report(&pump);
        if !partial.is_empty() {
            content.push_str("\n\n");
            content.push_str(&partial);
        }
        json!({
            "content": content,
            "is_error": true,
            "usage": pump.usage.to_json(),
        })
    } else if let Some(err) = pump.error {
        json!({
            "content": format!("handoff failed: {err}"),
            "is_error": true,
            "usage": pump.usage.to_json(),
        })
    } else {
        json!({
            "content": pump.text,
            "is_error": false,
            "usage": pump.usage.to_json(),
        })
    };
    let mut st = slot.state.lock().unwrap_or_else(|e| e.into_inner());
    st.finished = true;
    st.reply = Some(reply.clone());
    drop(st);
    slot.done.notify_all();
    reply
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
                    if !st.finished {
                        return json!({
                            "content": "sidekick busy; call handoff_wait",
                            "is_error": true,
                        });
                    }
                }
                map.insert(lead.clone(), slot.clone());
            }
            let qualified = active.sidekick.clone();
            let pump_slot = slot.clone();
            let cwd2 = cwd.clone();
            let lead2 = lead.clone();
            std::thread::spawn(move || {
                pump_handoff(&pump_slot, &task, &cwd2, &lead2, &qualified);
            });
            if !block {
                return json!({
                    "content": format!("handoff {} running in background; call handoff_wait to collect the report", slot.handoff_id),
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
            slots().remove(&lead);
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
            slots().remove(&lead);
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

fn tool_defs() -> Vec<Value> {
    vec![
        json!({
            "name": "handoff",
            "label": "handoff",
            "description": LEAD_PROMPT,
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

fn main() {
    let arg = std::env::args().nth(1);
    if arg.as_deref() == Some("manifest") {
        println!("{}", manifest());
        return;
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
                eprintln!("fusion probe: tools req session={}", &sess.to_string()[..sess.to_string().len().min(300)]);
                eprintln!("fusion probe: active={}", active(Some(&sess)).is_some());
                let tools = if active(Some(&sess)).is_some() {
                    tool_defs()
                } else {
                    Vec::new()
                };
                reply(id, json!({"tools": tools}));
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
}
