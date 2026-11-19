//! Read-only HTTP status service for the 5950x ARC CI node (`arc-runners`).
//!
//! Exposes `GET /healthz` and `GET /status` inside the cluster so lightweight
//! `route` jobs from any repository can determine in <10 ms whether heavy ARC
//! jobs are active on `k3d-5950x-ci-0`, without mounting Kubernetes API tokens
//! into CI runner pods or distributing cross-repository GitHub PATs.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use rustls_pki_types::CertificateDer;
use rustls_pki_types::pem::PemObject;
use serde::Serialize;
use serde_json::Value;

pub const DEFAULT_NAMESPACE: &str = "arc-runners";
pub const DEFAULT_API_URL: &str = "https://kubernetes.default.svc";
pub const DEFAULT_TOKEN_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/token";
pub const DEFAULT_CA_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/ca.crt";
pub const DEFAULT_LOADAVG_PATH: &str = "/proc/loadavg";
pub const DEFAULT_MEMINFO_PATH: &str = "/proc/meminfo";
pub const DEFAULT_MAX_CONCURRENCY: usize = 32;

pub const DEFAULT_LIGHT_SUFFIXES: &[&str] = &["-route", "-light", "-deploy"];
pub const DEFAULT_LIGHT_PREFIXES: &[&str] = &["sonarqube-"];
pub const TERMINAL_PHASES: &[&str] = &["Succeeded", "Failed", "Outdated"];

const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_K8S_BODY_BYTES: u64 = 16 * 1024 * 1024;
const WORKER_STACK_BYTES: usize = 256 * 1024;
const CLIENT_IO_TIMEOUT: Duration = Duration::from_secs(5);

static RUSTLS_PROVIDER_INIT: Once = Once::new();

#[derive(Debug, Clone)]
pub struct KubernetesAPIError(pub String);

impl fmt::Display for KubernetesAPIError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for KubernetesAPIError {}

pub fn parse_csv(value: Option<&str>) -> Vec<String> {
    match value {
        None => Vec::new(),
        Some(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect(),
    }
}

/// Match a string against a Unix shell-style glob (`*`, `?`, `[abc]`, `[!abc]`),
/// matching Python's `fnmatch.fnmatchcase` semantics.
pub fn fnmatch_case(text: &str, pattern: &str) -> bool {
    let t: Vec<char> = text.chars().collect();
    let p: Vec<char> = pattern.chars().collect();
    fnmatch_inner(&t, &p)
}

fn fnmatch_inner(t: &[char], p: &[char]) -> bool {
    let (mut ti, mut pi) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_ti: usize = 0;

    while ti < t.len() {
        if pi < p.len() {
            match p[pi] {
                '?' => {
                    ti += 1;
                    pi += 1;
                    continue;
                }
                '*' => {
                    star_pi = Some(pi);
                    pi += 1;
                    star_ti = ti;
                    continue;
                }
                '[' => {
                    if let Some((matched, next_pi)) = match_char_class(t[ti], &p[pi + 1..]) {
                        if matched {
                            ti += 1;
                            pi += 1 + next_pi;
                            continue;
                        }
                    } else if t[ti] == '[' {
                        ti += 1;
                        pi += 1;
                        continue;
                    }
                }
                ch if ch == t[ti] => {
                    ti += 1;
                    pi += 1;
                    continue;
                }
                _ => {}
            }
        }
        if let Some(sp) = star_pi {
            star_ti += 1;
            ti = star_ti;
            pi = sp + 1;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

fn match_char_class(ch: char, rest: &[char]) -> Option<(bool, usize)> {
    let mut idx = 0;
    let negate = if rest.first() == Some(&'!') {
        idx += 1;
        true
    } else {
        false
    };
    let start = idx;
    let mut matched = false;
    while idx < rest.len() {
        if rest[idx] == ']' && idx > start {
            return Some((if negate { !matched } else { matched }, idx + 1));
        }
        if idx + 2 < rest.len() && rest[idx + 1] == '-' && rest[idx + 2] != ']' {
            let lo = rest[idx];
            let hi = rest[idx + 2];
            if lo <= ch && ch <= hi {
                matched = true;
            }
            idx += 3;
        } else {
            if rest[idx] == ch {
                matched = true;
            }
            idx += 1;
        }
    }
    None
}

#[derive(Debug, Clone)]
pub struct LightPoolMatcher {
    pub patterns: Vec<String>,
    pub suffixes: Vec<String>,
    pub prefixes: Vec<String>,
}

impl Default for LightPoolMatcher {
    fn default() -> Self {
        Self {
            patterns: Vec::new(),
            suffixes: DEFAULT_LIGHT_SUFFIXES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            prefixes: DEFAULT_LIGHT_PREFIXES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

impl LightPoolMatcher {
    pub fn from_env_map(env: &HashMap<String, String>) -> Self {
        let patterns = parse_csv(env.get("LIGHT_SCALE_SETS").map(String::as_str));
        let suffixes = match env.get("LIGHT_SCALE_SET_SUFFIXES") {
            Some(val) => parse_csv(Some(val.as_str())),
            None => DEFAULT_LIGHT_SUFFIXES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        };
        let prefixes = match env.get("LIGHT_SCALE_SET_PREFIXES") {
            Some(val) => parse_csv(Some(val.as_str())),
            None => DEFAULT_LIGHT_PREFIXES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        };
        Self {
            patterns,
            suffixes,
            prefixes,
        }
    }

    pub fn is_light(&self, scale_set: &str) -> bool {
        if scale_set.is_empty() {
            return false;
        }
        if self.suffixes.iter().any(|s| scale_set.ends_with(s)) {
            return true;
        }
        if self.prefixes.iter().any(|p| scale_set.starts_with(p)) {
            return true;
        }
        self.patterns.iter().any(|pat| fnmatch_case(scale_set, pat))
    }
}

/// Read the 1-minute load average from `/proc/loadavg`.
pub fn read_load1(path: &Path) -> Option<f64> {
    let text = fs::read_to_string(path).ok()?;
    let first = text.split_whitespace().next()?;
    let val: f64 = first.parse().ok()?;
    val.is_finite().then_some(val)
}

/// Read `MemAvailable` (falling back to `MemFree`) in MiB from `/proc/meminfo`.
///
/// Note: inside k3d without lxcfs, `/proc/meminfo` reflects the underlying host
/// kernel (WSL2 VM) rather than the k3d node container's Docker memory limit.
pub fn read_mem_available_mib(path: &Path) -> Option<u64> {
    let text = fs::read_to_string(path).ok()?;
    let mut mem_free_kib: Option<u64> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("MemAvailable:") {
            let kib: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kib / 1024);
        } else if let Some(rest) = line.strip_prefix("MemFree:") {
            mem_free_kib = rest
                .split_whitespace()
                .next()
                .and_then(|tok| tok.parse::<u64>().ok());
        }
    }
    mem_free_kib.map(|kib| kib / 1024)
}

/// Build a mapping from ARC scale-set name to `spec.minRunners`.
pub fn extract_min_runners(items: &[Value]) -> HashMap<String, u64> {
    let mut result = HashMap::new();
    for item in items {
        let Some(obj) = item.as_object() else {
            continue;
        };
        let spec = obj.get("spec").and_then(Value::as_object);
        let meta = obj.get("metadata").and_then(Value::as_object);
        let labels = meta
            .and_then(|m| m.get("labels"))
            .and_then(Value::as_object);

        let min_runners = spec
            .and_then(|s| s.get("minRunners"))
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        let keys = [
            spec.and_then(|s| s.get("runnerScaleSetName"))
                .and_then(Value::as_str),
            labels
                .and_then(|l| l.get("actions.github.com/scale-set-name"))
                .and_then(Value::as_str),
            meta.and_then(|m| m.get("name")).and_then(Value::as_str),
        ];

        for key in keys.into_iter().flatten() {
            if !key.is_empty() {
                result.insert(key.to_owned(), min_runners);
            }
        }
    }
    result
}

fn runner_scale_set(er: &Value) -> &str {
    let Some(labels) = er
        .get("metadata")
        .and_then(|m| m.get("labels"))
        .and_then(Value::as_object)
    else {
        return "";
    };
    for key in [
        "actions.github.com/scale-set-name",
        "app.kubernetes.io/instance",
    ] {
        if let Some(val) = labels.get(key).and_then(Value::as_str)
            && !val.is_empty()
        {
            return val;
        }
    }
    ""
}

fn parse_workflow_run_id(raw: Option<&Value>) -> (Option<i64>, String) {
    match raw {
        Some(Value::Number(n)) => {
            if let Some(i) = n.as_i64()
                && i > 0
            {
                return (Some(i), i.to_string());
            }
            (None, String::new())
        }
        Some(Value::String(s)) => {
            let cleaned = s.trim();
            if cleaned.is_empty() {
                return (None, String::new());
            }
            match cleaned.parse::<i64>() {
                Ok(parsed) if parsed > 0 => (Some(parsed), cleaned.to_owned()),
                Ok(_) => (None, String::new()),
                Err(_) => (None, cleaned.to_owned()),
            }
        }
        _ => (None, String::new()),
    }
}

fn parse_job_id(raw: Option<&Value>) -> String {
    match raw {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(other) => other.to_string().trim().to_owned(),
    }
}

fn runner_repository(
    status: Option<&serde_json::Map<String, Value>>,
    labels: Option<&serde_json::Map<String, Value>>,
) -> String {
    if let Some(repo) = status
        .and_then(|s| s.get("jobRepositoryName"))
        .and_then(Value::as_str)
    {
        let trimmed = repo.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }
    let org = labels
        .and_then(|l| l.get("actions.github.com/organization"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let repo = labels
        .and_then(|l| l.get("actions.github.com/repository"))
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if !org.is_empty() && !repo.is_empty() {
        format!("{org}/{repo}")
    } else {
        repo.to_owned()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HeavyJob {
    pub name: String,
    pub scale_set: String,
    pub phase: String,
    pub job_id: String,
    pub job_name: String,
    pub repository: String,
    pub workflow_run_id: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StatusPayload {
    pub node: String,
    pub busy: bool,
    pub busy_count: usize,
    pub busy_by_set: BTreeMap<String, usize>,
    pub heavy_jobs: Vec<HeavyJob>,
    pub load1: Option<f64>,
    pub mem_available_mib: Option<u64>,
}

#[allow(clippy::too_many_arguments)]
pub fn evaluate_status(
    node: &str,
    ephemeral_runners: &[Value],
    min_runners_by_set: &HashMap<String, u64>,
    matcher: &LightPoolMatcher,
    exclude_runs: &HashSet<String>,
    exclude_jobs: &HashSet<String>,
    load1: Option<f64>,
    mem_available_mib: Option<u64>,
) -> StatusPayload {
    let mut heavy_jobs = Vec::new();

    for er in ephemeral_runners {
        let Some(obj) = er.as_object() else {
            continue;
        };
        let meta = obj.get("metadata").and_then(Value::as_object);
        let labels = meta
            .and_then(|m| m.get("labels"))
            .and_then(Value::as_object);
        let status = obj.get("status").and_then(Value::as_object);

        let scale_set = runner_scale_set(er);
        if matcher.is_light(scale_set) {
            continue;
        }

        let phase = status
            .and_then(|s| s.get("phase"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if TERMINAL_PHASES.contains(&phase) {
            continue;
        }

        let job_id = parse_job_id(status.and_then(|s| s.get("jobId")));

        let has_deletion_ts =
            meta.and_then(|m| m.get("deletionTimestamp"))
                .is_some_and(|v| match v {
                    Value::Null => false,
                    Value::String(s) => !s.is_empty(),
                    Value::Bool(b) => *b,
                    _ => true,
                });
        if has_deletion_ts && phase != "Running" && job_id.is_empty() {
            continue;
        }

        let min_runners = min_runners_by_set.get(scale_set).copied().unwrap_or(0);
        let is_active = (phase == "Running") || !job_id.is_empty() || (min_runners == 0);
        if !is_active {
            continue;
        }

        let job_name = status
            .and_then(|s| s.get("jobDisplayName"))
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_owned();
        let (workflow_run_id, run_id_str) =
            parse_workflow_run_id(status.and_then(|s| s.get("workflowRunId")));

        if !run_id_str.is_empty() && exclude_runs.contains(&run_id_str) {
            continue;
        }
        if !job_name.is_empty() && exclude_jobs.contains(&job_name) {
            continue;
        }

        let name = meta
            .and_then(|m| m.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned();

        heavy_jobs.push(HeavyJob {
            name,
            scale_set: scale_set.to_owned(),
            phase: phase.to_owned(),
            job_id,
            job_name,
            repository: runner_repository(status, labels),
            workflow_run_id,
        });
    }

    heavy_jobs.sort_by(|a, b| (&a.scale_set, &a.name).cmp(&(&b.scale_set, &b.name)));

    let mut busy_by_set = BTreeMap::new();
    for job in &heavy_jobs {
        *busy_by_set.entry(job.scale_set.clone()).or_insert(0) += 1;
    }

    let busy_count = heavy_jobs.len();
    StatusPayload {
        node: node.to_owned(),
        busy: busy_count > 0,
        busy_count,
        busy_by_set,
        heavy_jobs,
        load1,
        mem_available_mib,
    }
}

pub trait K8sBackend: Send + Sync {
    fn namespace(&self) -> &str;
    fn list_autoscaling_runner_sets(&self) -> Result<Vec<Value>, KubernetesAPIError>;
    fn list_ephemeral_runners(&self) -> Result<Vec<Value>, KubernetesAPIError>;
}

pub struct KubernetesClient {
    namespace: String,
    api_url: String,
    token_path: PathBuf,
    agent: ureq::Agent,
}

impl KubernetesClient {
    pub fn new(
        namespace: String,
        api_url: String,
        token_path: PathBuf,
        ca_path: PathBuf,
        timeout: Duration,
    ) -> Result<Self, KubernetesAPIError> {
        RUSTLS_PROVIDER_INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });

        let api_url = api_url.trim_end_matches('/').to_owned();
        let mut config_builder = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_idle_connections(2)
            .max_idle_connections_per_host(2);

        if api_url.starts_with("https://") && ca_path.is_file() {
            let pem_bytes = fs::read(&ca_path)
                .map_err(|e| KubernetesAPIError(format!("failed to read CA bundle: {e}")))?;
            let mut certs = Vec::new();
            for item in CertificateDer::pem_slice_iter(&pem_bytes) {
                let der = item.map_err(|e| {
                    KubernetesAPIError(format!("failed to parse CA bundle PEM: {e}"))
                })?;
                certs.push(ureq::tls::Certificate::from_der(der.as_ref()).to_owned());
            }
            if certs.is_empty() {
                return Err(KubernetesAPIError(
                    "CA bundle contains no valid certificates".to_owned(),
                ));
            }
            let tls = ureq::tls::TlsConfig::builder()
                .provider(ureq::tls::TlsProvider::Rustls)
                .root_certs(ureq::tls::RootCerts::new_with_certs(&certs))
                .build();
            config_builder = config_builder.tls_config(tls);
        }

        let agent = config_builder.build().new_agent();
        Ok(Self {
            namespace,
            api_url,
            token_path,
            agent,
        })
    }

    fn read_token(&self) -> Result<String, KubernetesAPIError> {
        let raw = fs::read_to_string(&self.token_path).map_err(|e| {
            KubernetesAPIError(format!("failed to read service account token: {e}"))
        })?;
        let token = raw.trim();
        if token.is_empty() {
            return Err(KubernetesAPIError(
                "service account token is empty".to_owned(),
            ));
        }
        Ok(token.to_owned())
    }

    pub fn get_json(
        &self,
        path: &str,
    ) -> Result<serde_json::Map<String, Value>, KubernetesAPIError> {
        let token = self.read_token()?;
        let url = format!("{}{}", self.api_url, path);

        let mut response = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Accept", "application/json")
            .call()
            .map_err(|e| {
                // Never expose headers/tokens in error messages.
                KubernetesAPIError(format!("kubernetes API {path} request failed: {e}"))
            })?;

        let status_code = response.status().as_u16();
        if status_code >= 400 {
            return Err(KubernetesAPIError(format!(
                "kubernetes API {path} returned HTTP {status_code}"
            )));
        }

        let body_bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_K8S_BODY_BYTES)
            .read_to_vec()
            .map_err(|e| {
                KubernetesAPIError(format!("kubernetes API {path} request failed: {e}"))
            })?;

        let payload: Value = serde_json::from_slice(&body_bytes).map_err(|_| {
            KubernetesAPIError(format!("kubernetes API {path} returned invalid JSON"))
        })?;

        match payload {
            Value::Object(map) => Ok(map),
            _ => Err(KubernetesAPIError(format!(
                "kubernetes API {path} returned non-object JSON"
            ))),
        }
    }
}

impl K8sBackend for KubernetesClient {
    fn namespace(&self) -> &str {
        &self.namespace
    }

    fn list_autoscaling_runner_sets(&self) -> Result<Vec<Value>, KubernetesAPIError> {
        let path = format!(
            "/apis/actions.github.com/v1alpha1/namespaces/{}/autoscalingrunnersets",
            self.namespace
        );
        let obj = self.get_json(&path)?;
        Ok(obj
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn list_ephemeral_runners(&self) -> Result<Vec<Value>, KubernetesAPIError> {
        let path = format!(
            "/apis/actions.github.com/v1alpha1/namespaces/{}/ephemeralrunners",
            self.namespace
        );
        let obj = self.get_json(&path)?;
        Ok(obj
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }
}

pub type ClockFn = Arc<dyn Fn() -> f64 + Send + Sync>;

struct ArsCache {
    min_runners: HashMap<String, u64>,
    fetched_at: Option<f64>,
}

pub struct StatusService {
    k8s: Arc<dyn K8sBackend>,
    node: String,
    matcher: LightPoolMatcher,
    ars_cache_seconds: f64,
    loadavg_path: PathBuf,
    meminfo_path: PathBuf,
    clock: ClockFn,
    cache: Mutex<ArsCache>,
}

impl StatusService {
    pub fn new(
        k8s: Arc<dyn K8sBackend>,
        node: String,
        matcher: LightPoolMatcher,
        ars_cache_seconds: f64,
        loadavg_path: PathBuf,
        meminfo_path: PathBuf,
        clock: ClockFn,
    ) -> Self {
        Self {
            k8s,
            node,
            matcher,
            ars_cache_seconds: ars_cache_seconds.max(0.0),
            loadavg_path,
            meminfo_path,
            clock,
            cache: Mutex::new(ArsCache {
                min_runners: HashMap::new(),
                fetched_at: None,
            }),
        }
    }

    pub fn node(&self) -> &str {
        &self.node
    }

    pub fn namespace(&self) -> &str {
        self.k8s.namespace()
    }

    fn get_min_runners(
        &self,
        force: bool,
    ) -> Result<(HashMap<String, u64>, bool), KubernetesAPIError> {
        let now = (self.clock)();
        {
            let guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            let valid = !force
                && self.ars_cache_seconds > 0.0
                && guard
                    .fetched_at
                    .is_some_and(|ts| (now - ts) < self.ars_cache_seconds);
            if valid {
                return Ok((guard.min_runners.clone(), false));
            }
        }

        let ars_items = self.k8s.list_autoscaling_runner_sets()?;
        let mapping = extract_min_runners(&ars_items);
        let fetched_at = (self.clock)();
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        guard.min_runners = mapping.clone();
        guard.fetched_at = Some(fetched_at);
        Ok((mapping, true))
    }

    pub fn status(
        &self,
        exclude_runs: &HashSet<String>,
        exclude_jobs: &HashSet<String>,
    ) -> Result<StatusPayload, KubernetesAPIError> {
        let (mut min_runners, refreshed) = self.get_min_runners(false)?;
        let ephemeral_runners = self.k8s.list_ephemeral_runners()?;

        if !refreshed {
            let unknown_heavy = ephemeral_runners.iter().any(|er| {
                let scale_set = runner_scale_set(er);
                !scale_set.is_empty()
                    && !self.matcher.is_light(scale_set)
                    && !min_runners.contains_key(scale_set)
            });
            if unknown_heavy {
                min_runners = self.get_min_runners(true)?.0;
            }
        }

        Ok(evaluate_status(
            &self.node,
            &ephemeral_runners,
            &min_runners,
            &self.matcher,
            exclude_runs,
            exclude_jobs,
            read_load1(&self.loadavg_path),
            read_mem_available_mib(&self.meminfo_path),
        ))
    }
}

pub fn default_monotonic_clock() -> ClockFn {
    let origin = Instant::now();
    Arc::new(move || origin.elapsed().as_secs_f64())
}

pub fn system_hostname() -> String {
    let mut buf = [0u8; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) };
    if rc == 0 {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        if let Ok(s) = std::str::from_utf8(&buf[..len]) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }
    "unknown".to_owned()
}

pub fn build_service_from_env_map(
    env: &HashMap<String, String>,
) -> Result<StatusService, KubernetesAPIError> {
    let namespace = env
        .get("POD_NAMESPACE")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_NAMESPACE)
        .to_owned();

    let node = env
        .get("NODE_NAME")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(system_hostname);

    let api_url = env
        .get("KUBERNETES_API_URL")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_API_URL)
        .to_owned();

    let timeout_secs: f64 = env
        .get("K8S_TIMEOUT_SECONDS")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v > 0.0)
        .unwrap_or(5.0);

    let ars_cache_seconds: f64 = env
        .get("ARS_CACHE_SECONDS")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(10.0);

    let token_path = PathBuf::from(
        env.get("K8S_TOKEN_PATH")
            .map(String::as_str)
            .unwrap_or(DEFAULT_TOKEN_PATH),
    );
    let ca_path = PathBuf::from(
        env.get("K8S_CA_PATH")
            .map(String::as_str)
            .unwrap_or(DEFAULT_CA_PATH),
    );

    let k8s = KubernetesClient::new(
        namespace,
        api_url,
        token_path,
        ca_path,
        Duration::from_secs_f64(timeout_secs),
    )?;

    Ok(StatusService::new(
        Arc::new(k8s),
        node,
        LightPoolMatcher::from_env_map(env),
        ars_cache_seconds,
        PathBuf::from(DEFAULT_LOADAVG_PATH),
        PathBuf::from(DEFAULT_MEMINFO_PATH),
        default_monotonic_clock(),
    ))
}

// ============================================================================
// Minimal zero-dependency HTTP/1.1 server with bounded concurrency
// ============================================================================

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b'%');
                    i += 1;
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn parse_query_sets(query: &str) -> (HashSet<String>, HashSet<String>) {
    let mut exclude_runs = HashSet::new();
    let mut exclude_jobs = HashSet::new();

    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (percent_decode(k), percent_decode(v)),
            None => continue,
        };
        let target = match k.as_str() {
            "exclude_run" => &mut exclude_runs,
            "exclude_job" => &mut exclude_jobs,
            _ => continue,
        };
        for token in v.split(',') {
            let cleaned = token.trim();
            if !cleaned.is_empty() {
                target.insert(cleaned.to_owned());
            }
        }
    }

    (exclude_runs, exclude_jobs)
}

fn write_json_response(
    stream: &mut TcpStream,
    status_line: &str,
    extra_header: Option<&str>,
    body: &[u8],
) {
    let mut header = format!(
        "HTTP/1.1 {status_line}\r\n\
         Content-Type: application/json\r\n\
         Cache-Control: no-store\r\n\
         Connection: close\r\n"
    );
    if let Some(h) = extra_header {
        header.push_str(h);
        header.push_str("\r\n");
    }
    header.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));

    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn send_overload_503(mut stream: TcpStream) {
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    write_json_response(
        &mut stream,
        "503 Service Unavailable",
        None,
        b"{\"error\":\"too many concurrent requests\"}\n",
    );
    // Half-close write side and briefly drain unread request bytes so Linux
    // sends FIN instead of RST when the socket closes.
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
    let mut drain = [0u8; 1024];
    let _ = stream.read(&mut drain);
}

fn handle_connection(mut stream: TcpStream, service: &StatusService) {
    let _ = stream.set_read_timeout(Some(CLIENT_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(CLIENT_IO_TIMEOUT));

    let peer = stream
        .peer_addr()
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "-".to_owned());

    let request_line = {
        let mut reader = BufReader::new((&stream).take(MAX_REQUEST_BYTES as u64));
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        // Drain headers up to empty line so clients don't receive TCP RST on close.
        let mut hdr = String::new();
        loop {
            hdr.clear();
            match reader.read_line(&mut hdr) {
                Ok(0) | Err(_) => break,
                Ok(_) if hdr == "\r\n" || hdr == "\n" => break,
                Ok(_) => {}
            }
        }
        line.trim_end_matches(&['\r', '\n'][..]).to_owned()
    };

    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        write_json_response(
            &mut stream,
            "400 Bad Request",
            None,
            b"{\"error\":\"bad request\"}\n",
        );
        return;
    };
    let Some(target) = parts.next() else {
        write_json_response(
            &mut stream,
            "400 Bad Request",
            None,
            b"{\"error\":\"bad request\"}\n",
        );
        return;
    };

    if method != "GET" {
        write_json_response(
            &mut stream,
            "405 Method Not Allowed",
            Some("Allow: GET"),
            b"{\"error\":\"method not allowed\"}\n",
        );
        println!("{peer} \"{request_line}\" 405");
        return;
    }

    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target, ""),
    };

    match path {
        "/healthz" => {
            write_json_response(&mut stream, "200 OK", None, b"{\"ok\":true}\n");
        }
        "/status" => {
            let (exclude_runs, exclude_jobs) = parse_query_sets(query);
            match service.status(&exclude_runs, &exclude_jobs) {
                Ok(payload) => {
                    let mut body = serde_json::to_vec(&payload).unwrap_or_else(|_| b"{}".to_vec());
                    body.push(b'\n');
                    write_json_response(&mut stream, "200 OK", None, &body);
                    println!("{peer} \"{request_line}\" 200");
                }
                Err(err) => {
                    eprintln!("error: {err}");
                    let err_body = serde_json::json!({
                        "error": err.0,
                        "node": service.node(),
                    });
                    let mut body = serde_json::to_vec(&err_body).unwrap_or_else(|_| b"{}".to_vec());
                    body.push(b'\n');
                    write_json_response(&mut stream, "503 Service Unavailable", None, &body);
                    println!("{peer} \"{request_line}\" 503");
                }
            }
        }
        _ => {
            write_json_response(
                &mut stream,
                "404 Not Found",
                None,
                b"{\"error\":\"not found\"}\n",
            );
            println!("{peer} \"{request_line}\" 404");
        }
    }
}

struct ConcurrencyLimiter {
    active: AtomicUsize,
    max: usize,
}

impl ConcurrencyLimiter {
    fn new(max: usize) -> Self {
        Self {
            active: AtomicUsize::new(0),
            max: max.max(1),
        }
    }

    fn try_acquire(self: &Arc<Self>) -> Option<PermitGuard> {
        let mut cur = self.active.load(Ordering::Acquire);
        loop {
            if cur >= self.max {
                return None;
            }
            match self.active.compare_exchange_weak(
                cur,
                cur + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(PermitGuard {
                        limiter: Arc::clone(self),
                    });
                }
                Err(actual) => cur = actual,
            }
        }
    }

    fn active_count(&self) -> usize {
        self.active.load(Ordering::Acquire)
    }
}

struct PermitGuard {
    limiter: Arc<ConcurrencyLimiter>,
}

impl Drop for PermitGuard {
    fn drop(&mut self) {
        self.limiter.active.fetch_sub(1, Ordering::AcqRel);
    }
}

pub fn serve(
    listener: TcpListener,
    service: Arc<StatusService>,
    max_concurrency: usize,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    let limiter = Arc::new(ConcurrencyLimiter::new(max_concurrency));
    let fd = listener.as_raw_fd();

    while !shutdown.load(Ordering::Acquire) {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, -1) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 || (pfd.revents & libc::POLLIN) == 0 {
            continue;
        }

        let stream = match listener.accept() {
            Ok((s, _)) => s,
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(err) => {
                eprintln!("accept error: {err}");
                continue;
            }
        };

        let Some(permit) = limiter.try_acquire() else {
            send_overload_503(stream);
            continue;
        };

        let svc = Arc::clone(&service);
        let spawned = thread::Builder::new()
            .stack_size(WORKER_STACK_BYTES)
            .spawn(move || {
                let _permit = permit;
                handle_connection(stream, &svc);
            });

        if let Err(err) = spawned {
            // `permit` was moved into closure and dropped on Err — zero leak.
            eprintln!("failed to spawn worker thread: {err}");
        }
    }

    // Drain in-flight requests briefly before exiting.
    let deadline = Instant::now() + Duration::from_secs(3);
    while limiter.active_count() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Condvar;
    use std::sync::atomic::AtomicU64;

    fn make_ars(name: &str, min_runners: Option<u64>) -> Value {
        let mut spec = serde_json::Map::new();
        spec.insert("runnerScaleSetName".to_owned(), json!(name));
        if let Some(m) = min_runners {
            spec.insert("minRunners".to_owned(), json!(m));
        }
        json!({
            "metadata": {
                "name": name,
                "labels": { "actions.github.com/scale-set-name": name }
            },
            "spec": Value::Object(spec)
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn make_er(
        name: &str,
        scale_set: &str,
        phase: &str,
        job_id: Option<&str>,
        job_name: Option<&str>,
        repository: Option<&str>,
        workflow_run_id: Option<Value>,
        org: &str,
        repo_label: &str,
        deletion_timestamp: Option<&str>,
    ) -> Value {
        let mut meta = serde_json::Map::new();
        meta.insert("name".to_owned(), json!(name));
        meta.insert(
            "labels".to_owned(),
            json!({
                "actions.github.com/scale-set-name": scale_set,
                "actions.github.com/organization": org,
                "actions.github.com/repository": repo_label,
            }),
        );
        if let Some(ts) = deletion_timestamp {
            meta.insert("deletionTimestamp".to_owned(), json!(ts));
        }

        let mut status = serde_json::Map::new();
        if !phase.is_empty() {
            status.insert("phase".to_owned(), json!(phase));
        }
        if let Some(v) = job_id {
            status.insert("jobId".to_owned(), json!(v));
        }
        if let Some(v) = job_name {
            status.insert("jobDisplayName".to_owned(), json!(v));
        }
        if let Some(v) = repository {
            status.insert("jobRepositoryName".to_owned(), json!(v));
        }
        if let Some(v) = workflow_run_id {
            status.insert("workflowRunId".to_owned(), v);
        }

        json!({
            "metadata": Value::Object(meta),
            "status": Value::Object(status)
        })
    }

    fn temp_dir(label: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let id = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ci-node-status-test-{}-{}-{id}",
            std::process::id(),
            label
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_light_pool_matcher_defaults_and_env_overrides() {
        let matcher = LightPoolMatcher::default();
        for light in [
            "leiloes-5950x-route",
            "idealista-5950x-route",
            "cars-spy-5950x-route",
            "idealista-5950x-light",
            "idealista-5950x-deploy",
            "ponte-5950x-deploy",
            "sonarqube-5950x",
            "sonarqube-gate",
        ] {
            assert!(matcher.is_light(light), "expected light: {light}");
        }
        for heavy in [
            "idealista-5950x",
            "idealista-5950x-build",
            "leiloes-ci",
            "cars-spy-5950x",
            "ponte-5950x",
            "pt309-5950x",
            "5950x-k8s",
            "",
        ] {
            assert!(!matcher.is_light(heavy), "expected heavy: {heavy}");
        }

        let mut env = HashMap::new();
        env.insert(
            "LIGHT_SCALE_SETS".to_owned(),
            "5950x-k8s, canary-* , ".to_owned(),
        );
        let custom = LightPoolMatcher::from_env_map(&env);
        assert!(custom.is_light("5950x-k8s"));
        assert!(custom.is_light("canary-test"));
        assert!(custom.is_light("leiloes-5950x-route"));
        assert!(!custom.is_light("leiloes-ci"));

        let mut strict_env = HashMap::new();
        strict_env.insert("LIGHT_SCALE_SETS".to_owned(), "only-this".to_owned());
        strict_env.insert("LIGHT_SCALE_SET_SUFFIXES".to_owned(), String::new());
        strict_env.insert("LIGHT_SCALE_SET_PREFIXES".to_owned(), String::new());
        let strict = LightPoolMatcher::from_env_map(&strict_env);
        assert!(strict.is_light("only-this"));
        assert!(!strict.is_light("leiloes-5950x-route"));
        assert!(!strict.is_light("sonarqube-5950x"));
    }

    #[test]
    fn test_proc_readers() {
        let dir = temp_dir("proc");
        let loadavg = dir.join("loadavg");
        let meminfo = dir.join("meminfo");
        fs::write(&loadavg, "1.42 0.98 0.50 2/412 9876\n").unwrap();
        fs::write(
            &meminfo,
            "MemTotal:       52428800 kB\nMemFree:        10485760 kB\nMemAvailable:   41943040 kB\n",
        )
        .unwrap();
        assert!((read_load1(&loadavg).unwrap() - 1.42).abs() < 1e-6);
        assert_eq!(read_mem_available_mib(&meminfo), Some(40960));

        let fallback = dir.join("meminfo-free");
        fs::write(&fallback, "MemFree:        2048000 kB\n").unwrap();
        assert_eq!(read_mem_available_mib(&fallback), Some(2000));

        let missing = dir.join("missing");
        assert_eq!(read_load1(&missing), None);
        assert_eq!(read_mem_available_mib(&missing), None);
        let _ = fs::remove_dir_all(&dir);
    }

    fn sample_min_runners() -> HashMap<String, u64> {
        extract_min_runners(&[
            make_ars("idealista-5950x", Some(0)),
            make_ars("idealista-5950x-build", Some(1)),
            make_ars("idealista-5950x-route", Some(1)),
            make_ars("idealista-5950x-light", Some(0)),
            make_ars("idealista-5950x-deploy", Some(0)),
            make_ars("leiloes-ci", Some(0)),
            make_ars("leiloes-5950x-route", Some(1)),
            make_ars("ponte-5950x", Some(1)),
            make_ars("cars-spy-5950x", None),
        ])
    }

    #[test]
    fn test_idle_warm_runners_are_not_busy() {
        let ers = vec![
            make_er(
                "ponte-5950x-runner-1",
                "ponte-5950x",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "idealista-5950x-build-runner-1",
                "idealista-5950x-build",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "leiloes-5950x-route-runner-1",
                "leiloes-5950x-route",
                "Running",
                Some("j-route"),
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "idealista-5950x-light-runner-1",
                "idealista-5950x-light",
                "Running",
                Some("j-light"),
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "idealista-5950x-deploy-runner-1",
                "idealista-5950x-deploy",
                "Running",
                Some("j-deploy"),
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "sonarqube-scan-runner-1",
                "sonarqube-scan",
                "Running",
                Some("j-sonar"),
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
        ];
        let res = evaluate_status(
            "k3d-5950x-ci-0",
            &ers,
            &sample_min_runners(),
            &LightPoolMatcher::default(),
            &HashSet::new(),
            &HashSet::new(),
            Some(0.25),
            Some(45000),
        );
        assert!(!res.busy);
        assert_eq!(res.busy_count, 0);
        assert!(res.busy_by_set.is_empty());
        assert!(res.heavy_jobs.is_empty());
        assert_eq!(res.load1, Some(0.25));
        assert_eq!(res.mem_available_mib, Some(45000));
    }

    #[test]
    fn test_warm_heavy_runner_active_when_running_or_job_id_set() {
        let ers = vec![
            make_er(
                "ponte-5950x-runner-idle",
                "ponte-5950x",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "ponte-5950x-runner-busy",
                "ponte-5950x",
                "Running",
                Some("uuid-ponte"),
                Some("test"),
                Some("Bafff/voice-call-translator-pt"),
                Some(json!(1001)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "idealista-5950x-build-runner-busy",
                "idealista-5950x-build",
                "Pending",
                Some("uuid-build"),
                Some("build"),
                Some("Bafff/idealista-tracker"),
                Some(json!(1002)),
                "Bafff",
                "ex",
                None,
            ),
        ];
        let res = evaluate_status(
            "k3d-5950x-ci-0",
            &ers,
            &sample_min_runners(),
            &LightPoolMatcher::default(),
            &HashSet::new(),
            &HashSet::new(),
            None,
            None,
        );
        assert!(res.busy);
        assert_eq!(res.busy_count, 2);
        assert_eq!(res.busy_by_set.get("idealista-5950x-build"), Some(&1));
        assert_eq!(res.busy_by_set.get("ponte-5950x"), Some(&1));
        let names: Vec<&str> = res.heavy_jobs.iter().map(|j| j.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "idealista-5950x-build-runner-busy",
                "ponte-5950x-runner-busy"
            ]
        );
    }

    #[test]
    fn test_min_runners_zero_and_terminal_phases_including_outdated() {
        let ers = vec![
            make_er(
                "idealista-5950x-runner-booting",
                "idealista-5950x",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "idealista-tracker",
                None,
            ),
            make_er(
                "cars-spy-5950x-runner-new",
                "cars-spy-5950x",
                "",
                None,
                None,
                None,
                None,
                "Bafff",
                "cars-spy",
                None,
            ),
            make_er(
                "leiloes-ci-runner-done",
                "leiloes-ci",
                "Succeeded",
                Some("uuid-done"),
                Some("test"),
                None,
                Some(json!(2001)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "leiloes-ci-runner-failed",
                "leiloes-ci",
                "Failed",
                Some("uuid-fail"),
                Some("test"),
                None,
                Some(json!(2002)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "leiloes-ci-runner-outdated",
                "leiloes-ci",
                "Outdated",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "ponte-5950x-runner-outdated",
                "ponte-5950x",
                "Outdated",
                Some("uuid-outdated"),
                Some("test"),
                None,
                Some(json!(2003)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "leiloes-ci-runner-cancelled",
                "leiloes-ci",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                Some("2026-11-19T18:00:00Z"),
            ),
        ];
        let res = evaluate_status(
            "k3d-5950x-ci-0",
            &ers,
            &sample_min_runners(),
            &LightPoolMatcher::default(),
            &HashSet::new(),
            &HashSet::new(),
            None,
            None,
        );
        assert!(res.busy);
        assert_eq!(res.busy_count, 2);
        assert_eq!(res.busy_by_set.get("cars-spy-5950x"), Some(&1));
        assert_eq!(res.busy_by_set.get("idealista-5950x"), Some(&1));
        assert_eq!(res.heavy_jobs[0].repository, "Bafff/cars-spy");
        assert_eq!(res.heavy_jobs[1].repository, "Bafff/idealista-tracker");
        assert_eq!(res.heavy_jobs[1].workflow_run_id, None);
    }

    #[test]
    fn test_exclude_run_and_exclude_job() {
        let ers = vec![
            make_er(
                "leiloes-ci-runner-proven",
                "leiloes-ci",
                "Running",
                Some("uuid-1"),
                Some("proven"),
                Some("Bafff/leiloes-radar"),
                Some(json!(555001)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "leiloes-ci-runner-self",
                "leiloes-ci",
                "Running",
                Some("uuid-2"),
                Some("test"),
                Some("Bafff/leiloes-radar"),
                Some(json!(555002)),
                "Bafff",
                "ex",
                None,
            ),
            make_er(
                "idealista-5950x-runner-other",
                "idealista-5950x",
                "Running",
                Some("uuid-3"),
                Some("pytest"),
                Some("Bafff/idealista-tracker"),
                Some(json!(999001)),
                "Bafff",
                "ex",
                None,
            ),
        ];
        let res = evaluate_status(
            "k3d-5950x-ci-0",
            &ers,
            &sample_min_runners(),
            &LightPoolMatcher::default(),
            &HashSet::from(["555002".to_owned()]),
            &HashSet::from(["proven".to_owned()]),
            None,
            None,
        );
        assert!(res.busy);
        assert_eq!(res.busy_count, 1);
        assert_eq!(res.heavy_jobs[0].name, "idealista-5950x-runner-other");

        let idle = evaluate_status(
            "k3d-5950x-ci-0",
            &ers,
            &sample_min_runners(),
            &LightPoolMatcher::default(),
            &HashSet::from(["555002".to_owned(), "999001".to_owned()]),
            &HashSet::from(["proven".to_owned()]),
            None,
            None,
        );
        assert!(!idle.busy);
        assert_eq!(idle.busy_count, 0);
    }

    type Blocker = Arc<(Mutex<(bool, bool)>, Condvar)>;

    struct FakeK8s {
        ars: Mutex<Vec<Value>>,
        ers: Mutex<Vec<Value>>,
        error: Mutex<Option<String>>,
        ars_calls: AtomicUsize,
        er_calls: AtomicUsize,
        blocker: Option<Blocker>,
    }

    impl FakeK8s {
        fn new(ars: Vec<Value>, ers: Vec<Value>) -> Self {
            Self {
                ars: Mutex::new(ars),
                ers: Mutex::new(ers),
                error: Mutex::new(None),
                ars_calls: AtomicUsize::new(0),
                er_calls: AtomicUsize::new(0),
                blocker: None,
            }
        }
    }

    impl K8sBackend for FakeK8s {
        fn namespace(&self) -> &str {
            "arc-runners"
        }

        fn list_autoscaling_runner_sets(&self) -> Result<Vec<Value>, KubernetesAPIError> {
            self.ars_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(err) = self.error.lock().unwrap().clone() {
                return Err(KubernetesAPIError(err));
            }
            Ok(self.ars.lock().unwrap().clone())
        }

        fn list_ephemeral_runners(&self) -> Result<Vec<Value>, KubernetesAPIError> {
            self.er_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(ref b) = self.blocker {
                let (lock, cv) = &**b;
                let mut state = lock.lock().unwrap();
                state.0 = true;
                cv.notify_all();
                while !state.1 {
                    state = cv.wait(state).unwrap();
                }
            }
            if let Some(err) = self.error.lock().unwrap().clone() {
                return Err(KubernetesAPIError(err));
            }
            Ok(self.ers.lock().unwrap().clone())
        }
    }

    #[test]
    fn test_ars_cache_ttl_and_unknown_set_refresh() {
        let now = Arc::new(Mutex::new(100.0_f64));
        let clock_now = Arc::clone(&now);
        let clock: ClockFn = Arc::new(move || *clock_now.lock().unwrap());

        let fake = Arc::new(FakeK8s::new(
            vec![make_ars("idealista-5950x", Some(0))],
            vec![make_er(
                "idealista-5950x-runner-1",
                "idealista-5950x",
                "Pending",
                None,
                None,
                None,
                None,
                "Bafff",
                "ex",
                None,
            )],
        ));
        let dir = temp_dir("cache");
        let svc = StatusService::new(
            Arc::clone(&fake) as Arc<dyn K8sBackend>,
            "k3d-5950x-ci-0".to_owned(),
            LightPoolMatcher::default(),
            10.0,
            dir.join("no-loadavg"),
            dir.join("no-meminfo"),
            clock,
        );

        assert!(svc.status(&HashSet::new(), &HashSet::new()).unwrap().busy);
        assert_eq!(fake.ars_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.er_calls.load(Ordering::SeqCst), 1);

        *now.lock().unwrap() = 105.0;
        let _ = svc.status(&HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(fake.ars_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.er_calls.load(Ordering::SeqCst), 2);

        fake.ars
            .lock()
            .unwrap()
            .push(make_ars("ponte-5950x", Some(1)));
        *fake.ers.lock().unwrap() = vec![make_er(
            "ponte-5950x-runner-1",
            "ponte-5950x",
            "Pending",
            None,
            None,
            None,
            None,
            "Bafff",
            "ex",
            None,
        )];
        *now.lock().unwrap() = 106.0;
        let third = svc.status(&HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(fake.ars_calls.load(Ordering::SeqCst), 2);
        assert!(!third.busy);

        *now.lock().unwrap() = 120.0;
        let _ = svc.status(&HashSet::new(), &HashSet::new()).unwrap();
        assert_eq!(fake.ars_calls.load(Ordering::SeqCst), 3);
        let _ = fs::remove_dir_all(&dir);
    }

    fn http_get(addr: std::net::SocketAddr, path: &str) -> (u16, Value) {
        let mut s = TcpStream::connect(addr).unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).unwrap();
        let mut resp = String::new();
        s.read_to_string(&mut resp).unwrap();
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        let code: u16 = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (code, serde_json::from_str(body).unwrap())
    }

    #[test]
    fn test_http_endpoints_filters_503_and_concurrency() {
        let dir = temp_dir("http");
        let loadavg = dir.join("loadavg");
        let meminfo = dir.join("meminfo");
        fs::write(&loadavg, "0.75 0.50 0.25 1/100 123\n").unwrap();
        fs::write(&meminfo, "MemAvailable:   32768000 kB\n").unwrap();

        let fake = Arc::new(FakeK8s::new(
            vec![
                make_ars("idealista-5950x", Some(0)),
                make_ars("leiloes-ci", Some(0)),
            ],
            vec![
                make_er(
                    "idealista-5950x-runner-a",
                    "idealista-5950x",
                    "Running",
                    Some("id-1"),
                    Some("pytest"),
                    Some("Bafff/idealista-tracker"),
                    Some(json!(44590432742_i64)),
                    "Bafff",
                    "ex",
                    None,
                ),
                make_er(
                    "leiloes-ci-runner-b",
                    "leiloes-ci",
                    "Running",
                    Some("id-2"),
                    Some("proven"),
                    Some("Bafff/leiloes-radar"),
                    Some(json!(44590432999_i64)),
                    "Bafff",
                    "ex",
                    None,
                ),
            ],
        ));
        let svc = Arc::new(StatusService::new(
            Arc::clone(&fake) as Arc<dyn K8sBackend>,
            "k3d-5950x-ci-0".to_owned(),
            LightPoolMatcher::default(),
            10.0,
            loadavg,
            meminfo,
            default_monotonic_clock(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_bg = Arc::clone(&shutdown);
        let svc_bg = Arc::clone(&svc);
        let handle = thread::spawn(move || {
            let _ = serve(listener, svc_bg, 32, &shutdown_bg);
        });

        let (code, health) = http_get(addr, "/healthz");
        assert_eq!(code, 200);
        assert_eq!(health, json!({"ok": true}));

        let (code, status) = http_get(addr, "/status");
        assert_eq!(code, 200);
        assert_eq!(status["node"], "k3d-5950x-ci-0");
        assert_eq!(status["busy"], true);
        assert_eq!(status["busy_count"], 2);
        assert_eq!(status["load1"], 0.75);
        assert_eq!(status["mem_available_mib"], 32000);

        let (code, filtered) = http_get(
            addr,
            "/status?exclude_run=44590432742&exclude_job=proven,other",
        );
        assert_eq!(code, 200);
        assert_eq!(filtered["busy"], false);
        assert_eq!(filtered["busy_count"], 0);

        let (code, repeated) = http_get(addr, "/status?exclude_job=proven&exclude_job=pytest");
        assert_eq!(code, 200);
        assert_eq!(repeated["busy"], false);

        let (code, missing) = http_get(addr, "/does-not-exist");
        assert_eq!(code, 404);
        assert!(missing.get("error").is_some());

        *fake.error.lock().unwrap() = Some("apiserver unreachable".to_owned());
        let (code, err) = http_get(addr, "/status");
        assert_eq!(code, 503);
        assert_eq!(err["node"], "k3d-5950x-ci-0");
        assert!(
            err["error"]
                .as_str()
                .unwrap()
                .contains("apiserver unreachable")
        );

        shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(addr);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_concurrency_limit_returns_503_and_recovers() {
        let blocker = Arc::new((Mutex::new((false, false)), Condvar::new()));
        let mut fake = FakeK8s::new(vec![], vec![]);
        fake.blocker = Some(Arc::clone(&blocker));
        let fake = Arc::new(fake);

        let dir = temp_dir("conc");
        let svc = Arc::new(StatusService::new(
            fake,
            "k3d-5950x-ci-0".to_owned(),
            LightPoolMatcher::default(),
            10.0,
            dir.join("none"),
            dir.join("none2"),
            default_monotonic_clock(),
        ));

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_bg = Arc::clone(&shutdown);
        let handle = thread::spawn(move || {
            let _ = serve(listener, svc, 1, &shutdown_bg);
        });

        let first = thread::spawn(move || http_get(addr, "/status"));
        {
            let (lock, cv) = &*blocker;
            let mut st = lock.lock().unwrap();
            while !st.0 {
                st = cv.wait(st).unwrap();
            }
        }

        let (code, body) = http_get(addr, "/healthz");
        assert_eq!(code, 503);
        assert!(
            body["error"]
                .as_str()
                .unwrap()
                .contains("too many concurrent requests")
        );

        {
            let (lock, cv) = &*blocker;
            let mut st = lock.lock().unwrap();
            st.1 = true;
            cv.notify_all();
        }
        let (first_code, _) = first.join().unwrap();
        assert_eq!(first_code, 200);

        let (code, health) = http_get(addr, "/healthz");
        assert_eq!(code, 200);
        assert_eq!(health, json!({"ok": true}));

        shutdown.store(true, Ordering::Release);
        let _ = TcpStream::connect(addr);
        handle.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_k8s_client_rotated_token_and_truncated_response() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let received_auth = Arc::new(Mutex::new(Vec::<String>::new()));
        let auth_bg = Arc::clone(&received_auth);

        let server = thread::spawn(move || {
            for (idx, stream) in listener.incoming().take(4).enumerate() {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 4096];
                let n = s.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                for line in req.lines() {
                    if let Some(val) = line
                        .strip_prefix("Authorization: ")
                        .or_else(|| line.strip_prefix("authorization: "))
                    {
                        auth_bg.lock().unwrap().push(val.trim().to_owned());
                    }
                }
                match idx {
                    0 | 1 => {
                        let body = r#"{"items":[{"metadata":{"name":"ok"}}]}"#;
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        s.write_all(resp.as_bytes()).unwrap();
                    }
                    2 => {
                        let body = r#"{"message":"forbidden"}"#;
                        let resp = format!(
                            "HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        s.write_all(resp.as_bytes()).unwrap();
                    }
                    _ => {
                        // Truncated body
                        let resp = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\n{\"items\":";
                        s.write_all(resp.as_bytes()).unwrap();
                    }
                }
            }
        });

        let dir = temp_dir("k8s");
        let token_file = dir.join("token");
        fs::write(&token_file, "secret-token-v1\n").unwrap();

        let client = KubernetesClient::new(
            "arc-runners".to_owned(),
            format!("http://127.0.0.1:{port}"),
            token_file.clone(),
            dir.join("no-ca"),
            Duration::from_secs(3),
        )
        .unwrap();

        assert_eq!(client.list_ephemeral_runners().unwrap().len(), 1);
        assert_eq!(
            received_auth.lock().unwrap().last().unwrap(),
            "Bearer secret-token-v1"
        );

        fs::write(&token_file, "secret-token-v2\n").unwrap();
        assert_eq!(client.list_autoscaling_runner_sets().unwrap().len(), 1);
        assert_eq!(
            received_auth.lock().unwrap().last().unwrap(),
            "Bearer secret-token-v2"
        );

        let err_403 = client.list_ephemeral_runners().unwrap_err();
        assert!(err_403.0.contains("403"));
        assert!(!err_403.0.contains("secret-token-v2"));

        let err_trunc = client.list_ephemeral_runners().unwrap_err();
        assert!(!err_trunc.0.contains("secret-token-v2"));

        server.join().unwrap();
        let _ = fs::remove_dir_all(&dir);
    }
}
