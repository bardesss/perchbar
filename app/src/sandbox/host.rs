//! Host functions exposed to widget code: storage, log and time.
//!
//! Each is installed as a hidden `__host_*` global that `prelude.js` captures
//! and deletes before widget code runs. Values cross the boundary as JSON text.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use chrono::{DateTime, Datelike, Offset, TimeZone, Timelike};
use rquickjs::{Ctx, Exception, Function};
use serde_json::{json, Map, Value};

use super::{permissions, HttpClient, HttpFuture, HttpRequest, HttpResponse};

/// Maximum serialized size of one instance's storage object.
pub const STORAGE_MAX_BYTES: usize = 64 * 1024;

/// Largest response body a widget can receive.
pub const RESPONSE_MAX_BYTES: usize = 2 * 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

tokio::task_local! {
    /// The hosts allowed for the request being sent on this task. The redirect
    /// policy runs inside the polling task, so it reads the list from here.
    /// Unset means nothing is allowed.
    static ALLOWED_HOSTS: Vec<String>;
}

/// The real HTTPS client: rustls, a 10 s timeout, a 2 MB body cap, and a
/// redirect policy that re-checks the allow-list on every hop (max 5).
pub struct ReqwestClient {
    client: reqwest::Client,
}

impl ReqwestClient {
    pub fn new() -> Self {
        let policy = reqwest::redirect::Policy::custom(|attempt| {
            let mut chain = attempt.previous().to_vec();
            chain.push(attempt.url().clone());
            if policy_decision(&chain) {
                attempt.follow()
            } else {
                attempt.error("Perchbar: redirect blocked")
            }
        });
        let client = reqwest::Client::builder()
            .use_rustls_tls()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("Perchbar/", env!("CARGO_PKG_VERSION")))
            .redirect(policy)
            // Never leak a previous URL (or its query string) via Referer.
            .referer(false)
            .build()
            .expect("http client configuration is valid");
        ReqwestClient { client }
    }

    async fn fetch(&self, req: HttpRequest) -> Result<HttpResponse, String> {
        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|_| format!("invalid method {:?}", req.method))?;
        let mut builder = self.client.request(method, &req.url);
        for (name, value) in &req.headers {
            builder = builder.header(name, value);
        }
        if let Some(body) = req.body {
            builder = builder.body(body);
        }
        let mut resp = builder.send().await.map_err(|e| describe(&e))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| describe(&e))? {
            if body.len() + chunk.len() > RESPONSE_MAX_BYTES {
                return Err(format!("response larger than {RESPONSE_MAX_BYTES} bytes"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpResponse {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }
}

/// Whether a redirect chain may continue, using the allow-list scoped to the
/// current request. Outside any scope nothing is allowed.
fn policy_decision(chain: &[url::Url]) -> bool {
    let allowed = ALLOWED_HOSTS.try_with(Clone::clone).unwrap_or_default();
    permissions::redirect_allowed(chain, &allowed)
}

impl Default for ReqwestClient {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpClient for ReqwestClient {
    fn send(&self, req: HttpRequest) -> HttpFuture<'_> {
        let allowed = req.allowed_hosts.clone();
        Box::pin(ALLOWED_HOSTS.scope(allowed, self.fetch(req)))
    }
}

/// An error message that includes the cause chain (reqwest hides redirect and
/// TLS causes behind a generic top-level message).
fn describe(e: &reqwest::Error) -> String {
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(e);
    while let Some(cause) = source {
        msg.push_str(": ");
        msg.push_str(&cause.to_string());
        source = cause.source();
    }
    msg
}

/// Each log message is cut to this many bytes.
pub const LOG_MESSAGE_MAX_BYTES: usize = 1024;
/// Log calls beyond this many per run are dropped.
pub const LOG_MAX_CALLS: usize = 200;

/// Caps widget logging per run so a noisy widget cannot flood the log.
#[derive(Debug, Default)]
pub struct LogLimiter {
    calls: usize,
}

impl LogLimiter {
    /// The text to log for this call, or `None` if it must be dropped.
    pub fn admit(&mut self, msg: &str) -> Option<String> {
        self.calls += 1;
        match self.calls {
            n if n <= LOG_MAX_CALLS => Some(truncate_to_boundary(msg, LOG_MESSAGE_MAX_BYTES)),
            n if n == LOG_MAX_CALLS + 1 => Some("log limit reached".to_string()),
            _ => None,
        }
    }
}

fn truncate_to_boundary(msg: &str, max: usize) -> String {
    if msg.len() <= max {
        return msg.to_string();
    }
    let mut end = max;
    while !msg.is_char_boundary(end) {
        end -= 1;
    }
    msg[..end].to_string()
}

/// One instance's persisted key/value store (a single JSON object on disk).
#[derive(Debug)]
pub struct Storage {
    path: PathBuf,
    values: Map<String, Value>,
    dirty: bool,
}

impl Storage {
    /// Loads `path`; a missing or unreadable file starts empty.
    pub fn load(path: PathBuf) -> Storage {
        let values = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                log::warn!("ignoring corrupt storage {}: {e}", path.display());
                Map::new()
            }),
            Err(_) => Map::new(),
        };
        Storage {
            path,
            values,
            dirty: false,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.values.get(key)
    }

    /// Stores `value`, rejecting it if the whole object would exceed the cap.
    pub fn set(&mut self, key: String, value: Value) -> Result<(), String> {
        let previous = self.values.insert(key.clone(), value);
        let size = serde_json::to_vec(&self.values).map_or(usize::MAX, |v| v.len());
        if size > STORAGE_MAX_BYTES {
            match previous {
                Some(p) => self.values.insert(key, p),
                None => self.values.remove(&key),
            };
            return Err(format!(
                "storage limit exceeded ({size} > {STORAGE_MAX_BYTES} bytes)"
            ));
        }
        self.dirty = true;
        Ok(())
    }

    /// Writes the store (tmp file + rename) if anything changed.
    pub fn save_if_dirty(&mut self) -> std::io::Result<()> {
        if !self.dirty {
            return Ok(());
        }
        write_atomic(&self.path, &serde_json::to_vec(&self.values)?)?;
        self.dirty = false;
        Ok(())
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `time.now(tz?)` fields; `None` uses the local time zone.
pub fn time_now(tz: Option<&str>) -> Result<Value, String> {
    let now = chrono::Utc::now();
    match tz {
        None => Ok(time_fields(now.with_timezone(&chrono::Local))),
        Some(name) => {
            let tz: chrono_tz::Tz = name
                .parse()
                .map_err(|_| format!("unknown time zone {name:?}"))?;
            Ok(time_fields(now.with_timezone(&tz)))
        }
    }
}

fn time_fields<Tz: TimeZone>(t: DateTime<Tz>) -> Value
where
    Tz::Offset: std::fmt::Display,
{
    json!({
        "year": t.year(),
        "month": t.month(),
        "day": t.day(),
        "hour": t.hour(),
        "minute": t.minute(),
        "second": t.second(),
        "weekday": t.weekday().to_string(),
        "hhmm": t.format("%H:%M").to_string(),
        "offsetMinutes": t.offset().fix().local_minus_utc() / 60,
    })
}

/// Installs `__host_storage_get`, `__host_storage_set`, `__host_log` and
/// `__host_time_now` on the global object.
pub fn install<'js>(
    ctx: &Ctx<'js>,
    storage: Arc<Mutex<Storage>>,
    instance_id: String,
) -> rquickjs::Result<()> {
    let globals = ctx.globals();

    let store = storage.clone();
    let get = Function::new(ctx.clone(), move |key: String| -> Option<String> {
        let store = store.lock().unwrap();
        store.get(&key).map(Value::to_string)
    })?;
    globals.set("__host_storage_get", get)?;

    let set = Function::new(
        ctx.clone(),
        move |ctx: Ctx<'js>, key: String, json: String| -> rquickjs::Result<()> {
            let value: Value = serde_json::from_str(&json)
                .map_err(|e| Exception::throw_type(&ctx, &format!("storage.set: {e}")))?;
            storage
                .lock()
                .unwrap()
                .set(key, value)
                .map_err(|e| Exception::throw_range(&ctx, &format!("storage.set: {e}")))
        },
    )?;
    globals.set("__host_storage_set", set)?;

    let limiter = Mutex::new(LogLimiter::default());
    let log = Function::new(ctx.clone(), move |msg: String| {
        if let Some(line) = limiter.lock().unwrap().admit(&msg) {
            log::info!(target: "widget", "[{instance_id}] {line}");
        }
    })?;
    globals.set("__host_log", log)?;

    let now = Function::new(
        ctx.clone(),
        |ctx: Ctx<'js>, tz: Option<String>| -> rquickjs::Result<String> {
            time_now(tz.as_deref())
                .map(|v| v.to_string())
                .map_err(|e| Exception::throw_type(&ctx, &e))
        },
    )?;
    globals.set("__host_time_now", now)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serves one canned raw HTTP response to each of `n` connections.
    async fn serve(response: Vec<u8>, n: usize) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..n {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let _ = sock.write_all(&response).await;
                let _ = sock.shutdown().await;
            }
        });
        format!("http://{addr}/")
    }

    fn get(url: &str, allowed: &[&str]) -> HttpRequest {
        HttpRequest {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
            body: None,
            allowed_hosts: allowed.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn chain(urls: &[&str]) -> Vec<url::Url> {
        urls.iter().map(|u| url::Url::parse(u).unwrap()).collect()
    }

    #[tokio::test]
    async fn policy_decision_reads_scoped_allow_list() {
        let ok = chain(&["https://api.coinbase.com/a", "https://api.coinbase.com/b"]);
        let evil = chain(&["https://api.coinbase.com/a", "https://evil.com/b"]);
        let (ok_in, evil_in) = ALLOWED_HOSTS
            .scope(vec!["api.coinbase.com".to_string()], async {
                (policy_decision(&ok), policy_decision(&evil))
            })
            .await;
        assert!(ok_in);
        assert!(!evil_in);
        assert!(!policy_decision(&ok), "no scope must deny everything");
    }

    #[tokio::test]
    async fn client_returns_small_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let url = serve(raw.to_vec(), 1).await;
        let resp = ReqwestClient::new().send(get(&url, &[])).await.unwrap();
        assert_eq!((resp.status, resp.body.as_str()), (200, "hi"));
    }

    #[tokio::test]
    async fn client_rejects_body_over_2mb() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            RESPONSE_MAX_BYTES + 1
        );
        let mut raw = head.into_bytes();
        raw.extend(std::iter::repeat_n(b'a', RESPONSE_MAX_BYTES + 1));
        let url = serve(raw, 1).await;
        let err = ReqwestClient::new().send(get(&url, &[])).await.unwrap_err();
        assert!(err.contains("larger than"), "{err}");
    }

    #[tokio::test]
    async fn client_blocks_redirect_to_undeclared_host() {
        let raw = "HTTP/1.1 302 Found\r\nLocation: https://evil.com/x\r\nContent-Length: 0\r\n\r\n";
        let url = serve(raw.as_bytes().to_vec(), 1).await;
        let err = ReqwestClient::new()
            .send(get(&url, &["127.0.0.1"]))
            .await
            .unwrap_err();
        assert!(err.contains("redirect blocked"), "{err}");
    }

    #[test]
    fn log_messages_are_truncated_on_a_char_boundary() {
        let mut l = LogLimiter::default();
        let long = "a".repeat(5000);
        assert_eq!(l.admit(&long).unwrap().len(), LOG_MESSAGE_MAX_BYTES);
        // 1023 ASCII bytes then a 2-byte char straddling the limit.
        let tricky = format!("{}é{}", "a".repeat(1023), "b".repeat(10));
        assert_eq!(l.admit(&tricky).unwrap().len(), 1023);
        assert_eq!(l.admit("short").as_deref(), Some("short"));
    }

    #[test]
    fn log_calls_are_capped_per_run() {
        let mut l = LogLimiter::default();
        for i in 0..LOG_MAX_CALLS {
            assert_eq!(l.admit("x").as_deref(), Some("x"), "call {i}");
        }
        assert_eq!(l.admit("x").as_deref(), Some("log limit reached"));
        assert_eq!(l.admit("x"), None);
        assert_eq!(l.admit("x"), None);
    }

    #[test]
    fn storage_cap_rejects_and_keeps_previous_value() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = Storage::load(dir.path().join("s.json"));
        s.set("k".into(), json!("small")).unwrap();
        assert!(s
            .set("k".into(), json!("x".repeat(STORAGE_MAX_BYTES)))
            .is_err());
        assert_eq!(s.get("k"), Some(&json!("small")));
    }

    #[test]
    fn unchanged_storage_is_not_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.json");
        Storage::load(path.clone()).save_if_dirty().unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn local_time_has_all_fields() {
        let v = time_now(None).unwrap();
        for k in [
            "year",
            "month",
            "day",
            "hour",
            "minute",
            "second",
            "weekday",
            "hhmm",
            "offsetMinutes",
        ] {
            assert!(v.get(k).is_some(), "{k}");
        }
        assert_eq!(v["hhmm"].as_str().unwrap().len(), 5);
    }
}
