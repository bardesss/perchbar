//! Each bundled template runs in the real sandbox against a fake HTTP client.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use perchbar::display::{Block, Color, Output};
use perchbar::manifest::{self, Manifest};
use perchbar::sandbox::{
    self, HttpClient, HttpFuture, HttpRequest, HttpResponse, Limits, RunContext, RunError,
};
use perchbar::setup;

/// Answers each request with the next queued body and records the URLs.
#[derive(Default)]
struct FakeHttp {
    bodies: Mutex<VecDeque<String>>,
    urls: Mutex<Vec<String>>,
}

impl FakeHttp {
    fn with(bodies: &[&str]) -> Arc<FakeHttp> {
        Arc::new(FakeHttp {
            bodies: Mutex::new(bodies.iter().map(|b| b.to_string()).collect()),
            urls: Mutex::new(Vec::new()),
        })
    }
}

impl HttpClient for FakeHttp {
    fn send(&self, req: HttpRequest) -> HttpFuture<'_> {
        self.urls.lock().unwrap().push(req.url.clone());
        let body = self.bodies.lock().unwrap().pop_front();
        Box::pin(async move {
            let body = body.ok_or("no more fake responses")?;
            Ok(HttpResponse {
                status: 200,
                headers: vec![("Content-Type".into(), "application/json".into())],
                body,
            })
        })
    }
}

/// The bundled templates installed into a temp widgets folder.
struct Installed {
    dir: tempfile::TempDir,
}

impl Installed {
    fn new() -> Installed {
        let dir = tempfile::tempdir().unwrap();
        setup::install_bundled(&dir.path().join("widgets")).unwrap();
        Installed { dir }
    }

    fn widget_dir(&self, id: &str) -> PathBuf {
        self.dir.path().join("widgets").join(id)
    }

    fn manifest(&self, id: &str) -> Manifest {
        manifest::load(&self.widget_dir(id)).unwrap()
    }

    /// Manifest defaults overlaid with `overrides`.
    fn settings(&self, id: &str, overrides: Value) -> Map<String, Value> {
        let mut out: Map<String, Value> = self
            .manifest(id)
            .settings
            .iter()
            .filter_map(|f| f.default.clone().map(|d| (f.key.clone(), d)))
            .collect();
        if let Value::Object(o) = overrides {
            out.extend(o);
        }
        out
    }

    async fn run(
        &self,
        id: &str,
        overrides: Value,
        http: Arc<FakeHttp>,
    ) -> Result<Output, RunError> {
        sandbox::run(RunContext {
            manifest: Arc::new(self.manifest(id)),
            widget_dir: self.widget_dir(id),
            instance_id: "inst-00000001".into(),
            settings: self.settings(id, overrides),
            http,
            storage_path: self.dir.path().join("storage").join("inst-00000001.json"),
            limits: Limits::default(),
        })
        .await
    }
}

const PRICE: &str = r#"{"data":{"amount":"67120.41"}}"#;
const LOWER_PRICE: &str = r#"{"data":{"amount":"66000.00"}}"#;

/// The text blocks of the first frame, as (value, color).
fn texts(out: &Output) -> Vec<(String, Color)> {
    out.frames[0]
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { value, color, .. } => Some((value.clone(), *color)),
            _ => None,
        })
        .collect()
}

fn price_color(out: &Output) -> Color {
    texts(out)
        .into_iter()
        .find(|(v, _)| v.starts_with('$'))
        .map(|(_, c)| c)
        .expect("no price text")
}

#[tokio::test]
async fn bitcoin_template_renders_price() {
    let w = Installed::new();
    let out = w
        .run("bitcoin-price", json!({}), FakeHttp::with(&[PRICE]))
        .await
        .unwrap();
    let blocks = &out.frames[0].blocks;
    assert!(
        matches!(&blocks[0], Block::Icon { name: Some(n), .. } if n == "bitcoin"),
        "{blocks:?}"
    );
    assert!(
        texts(&out).iter().any(|(v, _)| v == "$67,120"),
        "{blocks:?}"
    );
    let tooltip = out.frames[0].tooltip.as_deref().unwrap();
    assert!(
        tooltip.starts_with("BTC-USD $67,120.41 \u{b7} updated "),
        "{tooltip}"
    );
}

#[tokio::test]
async fn bitcoin_template_shows_symbol_for_other_coins() {
    let w = Installed::new();
    let out = w
        .run(
            "bitcoin-price",
            json!({"coin": "ETH", "currency": "EUR"}),
            FakeHttp::with(&[r#"{"data":{"amount":"3120.5"}}"#]),
        )
        .await
        .unwrap();
    let t = texts(&out);
    assert_eq!(t[0].0, "ETH");
    assert!(t.iter().any(|(v, _)| v == "\u{20ac}3,121"), "{t:?}");
}

#[tokio::test]
async fn bitcoin_template_colors_down_on_drop() {
    let w = Installed::new();
    let http = FakeHttp::with(&[PRICE, LOWER_PRICE]);
    let first = w
        .run("bitcoin-price", json!({}), http.clone())
        .await
        .unwrap();
    assert_eq!(price_color(&first), Color::Default);
    let second = w.run("bitcoin-price", json!({}), http).await.unwrap();
    assert_eq!(price_color(&second), Color::Down);
    // Two stored points: the sparkline appears.
    assert!(
        second.frames[0]
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Spark { values, .. } if values.len() == 2)),
        "{second:?}"
    );
}

#[tokio::test]
async fn bitcoin_template_only_calls_coinbase() {
    let w = Installed::new();
    let http = FakeHttp::with(&[PRICE, PRICE]);
    w.run("bitcoin-price", json!({}), http.clone())
        .await
        .unwrap();
    w.run(
        "bitcoin-price",
        json!({"coin": "SOL", "currency": "GBP"}),
        http.clone(),
    )
    .await
    .unwrap();
    let urls = http.urls.lock().unwrap().clone();
    assert_eq!(urls.len(), 2);
    for u in &urls {
        let parsed = url::Url::parse(u).unwrap();
        assert_eq!(parsed.scheme(), "https", "{u}");
        assert_eq!(parsed.host_str(), Some("api.coinbase.com"), "{u}");
    }
    assert!(urls[1].ends_with("/v2/prices/SOL-GBP/spot"), "{urls:?}");
}

#[tokio::test]
async fn bitcoin_template_reports_missing_price_clearly() {
    let w = Installed::new();
    for body in [
        r#"{"errors":[{"id":"not_found"}]}"#,
        "null",
        r#"{"data":null}"#,
    ] {
        let res = w
            .run("bitcoin-price", json!({}), FakeHttp::with(&[body]))
            .await;
        match res {
            Err(RunError::Script(m)) => {
                assert!(
                    m.contains("Coinbase sent no price for BTC-USD"),
                    "{body}: {m}"
                )
            }
            other => panic!("{body}: expected Script error, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn bitcoin_template_tooltip_has_four_decimals_below_one() {
    let w = Installed::new();
    let out = w
        .run(
            "bitcoin-price",
            json!({"coin": "DOGE"}),
            FakeHttp::with(&[r#"{"data":{"amount":"0.123456"}}"#]),
        )
        .await
        .unwrap();
    let tooltip = out.frames[0].tooltip.as_deref().unwrap();
    assert!(tooltip.starts_with("DOGE-USD $0.1235 \u{b7} "), "{tooltip}");
}

#[tokio::test]
async fn world_clock_two_cities_rotates() {
    let w = Installed::new();
    let out = w
        .run("world-clock", json!({}), FakeHttp::with(&[]))
        .await
        .unwrap();
    assert_eq!(out.frames.len(), 2);
    assert_eq!(out.rotate, Some(6));
    let tokyo = out.frames[1].tooltip.as_deref().unwrap();
    assert!(tokyo.starts_with("Tokyo \u{b7} "), "{tokyo}");
    assert!(tokyo.ends_with(" \u{b7} UTC+9"), "{tokyo}");
    assert!(texts(&out)[0].0.starts_with("Amsterdam "));
}

#[tokio::test]
async fn world_clock_one_city_is_one_frame() {
    let w = Installed::new();
    let out = w
        .run(
            "world-clock",
            json!({"cities": ["Tokyo|Asia/Tokyo"]}),
            FakeHttp::with(&[]),
        )
        .await
        .unwrap();
    assert_eq!(out.frames.len(), 1);
    assert_eq!(out.rotate, None);
    let t = &texts(&out)[0].0;
    assert!(
        t.starts_with("Tokyo ") && t.len() == "Tokyo 22:05".len(),
        "{t}"
    );
}

#[tokio::test]
async fn world_clock_bad_timezone_is_error() {
    let w = Installed::new();
    let res = w
        .run(
            "world-clock",
            json!({"cities": ["X|Mars/Base"]}),
            FakeHttp::with(&[]),
        )
        .await;
    match res {
        Err(RunError::Script(m)) => assert!(m.contains("Mars/Base"), "{m}"),
        other => panic!("expected Script error, got {other:?}"),
    }
}

fn line_count(path: &Path) -> usize {
    std::fs::read_to_string(path).unwrap().lines().count()
}

#[test]
fn bundled_templates_pass_manifest_validation() {
    let w = Installed::new();
    let ids: Vec<&str> = setup::BUNDLED.iter().map(|t| t.id).collect();
    assert_eq!(ids, ["bitcoin-price", "world-clock"]);
    for id in ids {
        let dir = w.widget_dir(id);
        let m = manifest::load(&dir).unwrap_or_else(|e| panic!("{id}: {e}"));
        assert_eq!(m.id, id);
        assert!(line_count(&dir.join("widget.js")) < 50, "{id}/widget.js");
        let readme = line_count(&dir.join("README.md"));
        assert!(
            (5..=10).contains(&readme),
            "{id}/README.md has {readme} lines"
        );
    }
}
