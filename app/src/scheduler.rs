//! Runs each visible widget instance on its refresh interval and publishes
//! the resulting `StripModel` whenever it changes.
//!
//! One tokio task per visible instance: wait, run, record the result, repeat.
//! Because the task awaits each run before looping, runs of one instance can
//! never overlap. `refresh_now` uses `Notify::notify_one`, which stores at most
//! one permit, so a request made during a run queues exactly one follow-up.
//! At most `MAX_CONCURRENT_RUNS` runs execute at once across all instances.
//!
//! All waiting is done with timers (no polling), so an idle scheduler uses no CPU.
//!
//! Must be started from inside a tokio runtime. The `publish` callback runs
//! while the scheduler's lock is held, so it must be quick and must not call
//! back into the `SchedulerHandle`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinHandle;

use crate::display::Output;
use crate::instances::{self, Instances};
use crate::manifest::Manifest;
use crate::paths;
use crate::sandbox::{self, HttpClient, Limits, RunContext, RunError};
use crate::strip::model::{Member, Slot, SlotKind, SlotState, StripModel};

const MAX_CONCURRENT_RUNS: usize = 4;
const MAX_BACKOFF_SECS: u64 = 600;
const MAX_START_JITTER_MS: u64 = 2000;

pub type RunFuture = Pin<Box<dyn Future<Output = Result<Output, RunError>> + Send>>;
/// Performs one widget run. Injectable so tests can fake the sandbox.
pub type Runner = Arc<dyn Fn(RunContext) -> RunFuture + Send + Sync>;

/// The real runner. Widget JS is synchronous and can occupy a thread for up to
/// the 10 s timeout, so each run goes to tokio's blocking pool (via
/// `spawn_blocking`, driving the sandbox future with `Handle::block_on`)
/// instead of stalling an async worker thread. Call from inside a runtime.
pub fn sandbox_runner() -> Runner {
    let handle = tokio::runtime::Handle::current();
    Arc::new(move |ctx: RunContext| {
        let handle = handle.clone();
        Box::pin(async move {
            tokio::task::spawn_blocking(move || handle.block_on(sandbox::run(ctx)))
                .await
                .unwrap_or_else(|e| Err(RunError::Script(format!("run aborted: {e}"))))
        })
    })
}

/// Wait before the next run: `refresh` seconds with no failures, otherwise
/// `refresh * 2^failures` capped at 600 seconds.
pub fn next_delay(refresh: u32, consecutive_failures: u32) -> Duration {
    if consecutive_failures == 0 {
        return Duration::from_secs(u64::from(refresh));
    }
    let factor = 1u64 << consecutive_failures.min(20);
    Duration::from_secs((u64::from(refresh) * factor).min(MAX_BACKOFF_SECS))
}

struct Running {
    notify: Arc<Notify>,
    task: JoinHandle<()>,
}

struct Inner {
    data: Instances,
    states: HashMap<String, SlotState>,
    model: StripModel,
    tasks: HashMap<String, Running>,
}

struct Shared {
    widgets: HashMap<String, (Arc<Manifest>, Arc<PathBuf>)>,
    http: Arc<dyn HttpClient>,
    runner: Runner,
    semaphore: Semaphore,
    instances_path: PathBuf,
    publish: Box<dyn Fn(StripModel) + Send + Sync>,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Rebuilds the slots; bumps the generation and publishes only if they differ.
    fn republish(&self, inner: &mut Inner) {
        let slots = self.build_slots(inner);
        if slots != inner.model.slots {
            inner.model.slots = slots;
            inner.model.generation += 1;
            (self.publish)(inner.model.clone());
        }
    }

    fn is_visible(&self, inst: &instances::Instance) -> bool {
        !inst.hidden && self.widgets.contains_key(&inst.widget)
    }

    fn build_slots(&self, inner: &Inner) -> Vec<Slot> {
        let find = |id: &str| inner.data.instances.iter().find(|i| i.id == id);
        let state_of = |id: &str| inner.states.get(id).cloned().unwrap_or(SlotState::Loading);
        let dir_of = |inst: &instances::Instance| self.widgets[&inst.widget].1.clone();
        let mut slots = Vec::new();
        for id in &inner.data.order {
            if let Some(inst) = find(id).filter(|i| self.is_visible(i)) {
                slots.push(Slot {
                    id: id.clone(),
                    kind: SlotKind::Single {
                        instance_id: id.clone(),
                        widget_dir: dir_of(inst),
                        state: state_of(id),
                        max_width: inst.max_width,
                    },
                });
            } else if let Some(group) = inner.data.groups.iter().find(|g| &g.id == id) {
                let members: Vec<Member> = group
                    .members
                    .iter()
                    .filter_map(|m| find(m).filter(|i| self.is_visible(i)))
                    .map(|inst| Member {
                        instance_id: inst.id.clone(),
                        widget_dir: dir_of(inst),
                        state: state_of(&inst.id),
                    })
                    .collect();
                if !members.is_empty() {
                    slots.push(Slot {
                        id: id.clone(),
                        kind: SlotKind::Group {
                            interval: group.interval,
                            members,
                        },
                    });
                }
            }
        }
        slots
    }
}

pub struct Scheduler;

impl Scheduler {
    pub fn start(
        widgets: HashMap<String, (Arc<Manifest>, PathBuf)>,
        instances: Instances,
        http: Arc<dyn HttpClient>,
        publish: impl Fn(StripModel) + Send + Sync + 'static,
    ) -> SchedulerHandle {
        Self::start_with_runner(
            widgets,
            instances,
            http,
            sandbox_runner(),
            paths::instances_file(),
            publish,
        )
    }

    pub fn start_with_runner(
        widgets: HashMap<String, (Arc<Manifest>, PathBuf)>,
        instances: Instances,
        http: Arc<dyn HttpClient>,
        runner: Runner,
        instances_path: PathBuf,
        publish: impl Fn(StripModel) + Send + Sync + 'static,
    ) -> SchedulerHandle {
        let shared = Arc::new(Shared {
            widgets: widgets
                .into_iter()
                .map(|(id, (m, dir))| (id, (m, Arc::new(dir))))
                .collect(),
            http,
            runner,
            semaphore: Semaphore::new(MAX_CONCURRENT_RUNS),
            instances_path,
            publish: Box::new(publish),
            inner: Mutex::new(Inner {
                data: instances,
                states: HashMap::new(),
                model: StripModel::default(),
                tasks: HashMap::new(),
            }),
        });
        {
            let mut inner = shared.lock();
            let visible: Vec<String> = inner
                .data
                .instances
                .iter()
                .filter(|i| shared.is_visible(i))
                .map(|i| i.id.clone())
                .collect();
            for id in visible {
                spawn_instance(&shared, &mut inner, id);
            }
            // The first model (everything Loading) always goes out.
            inner.model.slots = shared.build_slots(&inner);
            inner.model.generation = 1;
            (shared.publish)(inner.model.clone());
        }
        SchedulerHandle { shared }
    }
}

fn spawn_instance(shared: &Arc<Shared>, inner: &mut Inner, id: String) {
    inner.states.insert(id.clone(), SlotState::Loading);
    let notify = Arc::new(Notify::new());
    let task = tokio::spawn(instance_loop(shared.clone(), id.clone(), notify.clone()));
    inner.tasks.insert(id, Running { notify, task });
}

async fn instance_loop(shared: Arc<Shared>, id: String, notify: Arc<Notify>) {
    let mut wait = Duration::from_millis(rand::random_range(0..=MAX_START_JITTER_MS));
    let mut failures = 0u32;
    let mut last_good: Option<Output> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = notify.notified() => {}
        }
        let Some((ctx, refresh)) = make_context(&shared, &id) else {
            return;
        };
        let result = match shared.semaphore.acquire().await {
            Ok(_permit) => (shared.runner)(ctx).await,
            Err(_) => return,
        };
        let state = match result {
            Ok(output) => {
                failures = 0;
                last_good = Some(output.clone());
                SlotState::Ok(output)
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                log::warn!("widget run failed for {id}: {e}");
                match &last_good {
                    Some(prev) => SlotState::Stale(prev.clone()),
                    None => SlotState::Error {
                        message: e.to_string(),
                        last: None,
                    },
                }
            }
        };
        {
            let mut inner = shared.lock();
            // Skip if the instance was hidden or removed while the run was in flight.
            if let Some(slot) = inner.states.get_mut(&id) {
                *slot = state;
                shared.republish(&mut inner);
            }
        }
        wait = next_delay(refresh, failures);
    }
}

/// Reads the instance's current settings so edits apply on the next run.
fn make_context(shared: &Shared, id: &str) -> Option<(RunContext, u32)> {
    let inner = shared.lock();
    let inst = inner.data.instances.iter().find(|i| i.id == id)?;
    let (manifest, dir) = shared.widgets.get(&inst.widget)?;
    let ctx = RunContext {
        manifest: manifest.clone(),
        widget_dir: dir.as_ref().clone(),
        instance_id: id.to_string(),
        settings: inner.data.effective_settings(inst, manifest),
        http: shared.http.clone(),
        storage_path: paths::storage_file(id),
        limits: Limits::default(),
    };
    Some((ctx, manifest.refresh))
}

pub struct SchedulerHandle {
    shared: Arc<Shared>,
}

impl SchedulerHandle {
    /// Runs the instance now. If a run is already in flight, exactly one more
    /// run follows it. Hidden or unknown instances are ignored.
    pub fn refresh_now(&self, instance_id: &str) {
        if let Some(running) = self.shared.lock().tasks.get(instance_id) {
            running.notify.notify_one();
        }
    }

    /// Hides or shows an instance, saves `instances.json` and republishes.
    /// Hidden instances are not run; showing one starts it as Loading.
    pub fn set_hidden(&self, instance_id: &str, hidden: bool) {
        let shared = &self.shared;
        let mut inner = shared.lock();
        let Some(inst) = inner
            .data
            .instances
            .iter_mut()
            .find(|i| i.id == instance_id)
        else {
            return;
        };
        if inst.hidden == hidden {
            return;
        }
        inst.hidden = hidden;
        let widget_known = shared.widgets.contains_key(&inst.widget);
        if let Err(e) = instances::save(&shared.instances_path, &inner.data) {
            log::warn!("cannot save instances: {e}");
        }
        if hidden {
            inner.states.remove(instance_id);
            if let Some(running) = inner.tasks.remove(instance_id) {
                running.task.abort();
            }
        } else if widget_known && !inner.tasks.contains_key(instance_id) {
            spawn_instance(shared, &mut inner, instance_id.to_string());
        }
        shared.republish(&mut inner);
    }

    /// Stops all instance tasks. A run already on the blocking pool finishes
    /// on its own and its result is discarded.
    pub fn shutdown(self) {
        let mut inner = self.shared.lock();
        for (_, running) in inner.tasks.drain() {
            running.task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::display::{Block, Color, Frame, TextSize};
    use crate::instances::{Group, Instance};
    use crate::sandbox::{HttpFuture, HttpRequest};
    use crate::strip::model::{Member, SlotKind, SlotState};
    use serde_json::{json, Map};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct NoHttp;
    impl HttpClient for NoHttp {
        fn send(&self, _req: HttpRequest) -> HttpFuture<'_> {
            Box::pin(std::future::pending())
        }
    }

    fn out(text: &str) -> Output {
        Output {
            frames: vec![Frame {
                blocks: vec![Block::Text {
                    value: text.into(),
                    color: Color::Default,
                    size: TextSize::Normal,
                }],
                tooltip: None,
            }],
            rotate: None,
            click: None,
        }
    }

    fn manifest() -> Arc<Manifest> {
        Arc::new(
            serde_json::from_value(json!({
                "id": "w", "name": "W", "formatVersion": 1, "refresh": 60
            }))
            .unwrap(),
        )
    }

    fn inst(n: u32, hidden: bool) -> Instance {
        Instance {
            id: id(n),
            widget: "w".into(),
            settings: Map::new(),
            hidden,
            max_width: None,
        }
    }

    fn id(n: u32) -> String {
        format!("inst-{n:08x}")
    }

    type Published = Arc<Mutex<Vec<StripModel>>>;

    struct Rig {
        handle: SchedulerHandle,
        published: Published,
        path: PathBuf,
        _dir: tempfile::TempDir,
    }

    impl Rig {
        fn start(
            data: Instances,
            f: impl Fn(&str) -> Result<Output, RunError> + Send + Sync + 'static,
        ) -> Rig {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("instances.json");
            let widgets = HashMap::from([("w".to_string(), (manifest(), PathBuf::from("wdir")))]);
            let runner: Runner = Arc::new(move |ctx: RunContext| {
                let r = f(&ctx.instance_id);
                Box::pin(async move { r })
            });
            let published: Published = Arc::default();
            let sink = published.clone();
            let handle = Scheduler::start_with_runner(
                widgets,
                data,
                Arc::new(NoHttp),
                runner,
                path.clone(),
                move |m| sink.lock().unwrap().push(m),
            );
            Rig {
                handle,
                published,
                path,
                _dir: dir,
            }
        }

        fn last(&self) -> StripModel {
            self.published.lock().unwrap().last().cloned().unwrap()
        }

        fn count(&self) -> usize {
            self.published.lock().unwrap().len()
        }
    }

    async fn advance(secs: u64) {
        tokio::time::sleep(Duration::from_secs(secs)).await;
    }

    fn single_state(m: &StripModel, i: usize) -> SlotState {
        match &m.slots[i].kind {
            SlotKind::Single { state, .. } => state.clone(),
            other => panic!("not single: {other:?}"),
        }
    }

    fn data(instances: Vec<Instance>) -> Instances {
        let order = instances.iter().map(|i| i.id.clone()).collect();
        Instances {
            instances,
            groups: vec![],
            order,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(next_delay(60, 0), Duration::from_secs(60));
        assert_eq!(next_delay(60, 1), Duration::from_secs(120));
        assert_eq!(next_delay(60, 3), Duration::from_secs(480));
        assert_eq!(next_delay(60, 4), Duration::from_secs(600));
        assert_eq!(next_delay(10, 10), Duration::from_secs(600));
        assert_eq!(next_delay(10, 1000), Duration::from_secs(600));
    }

    #[tokio::test(start_paused = true)]
    async fn failure_keeps_last_output_stale() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let rig = Rig::start(data(vec![inst(1, false)]), move |_| {
            match c.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(out("one")),
                1 => Err(RunError::Timeout),
                _ => Ok(out("two")),
            }
        });
        assert_eq!(single_state(&rig.last(), 0), SlotState::Loading);
        advance(3).await;
        assert_eq!(single_state(&rig.last(), 0), SlotState::Ok(out("one")));
        advance(60).await;
        // Failure with a previous good output: Stale(previous), not Error.
        assert_eq!(single_state(&rig.last(), 0), SlotState::Stale(out("one")));
        // One failure doubles the wait: nothing yet after 60 more seconds.
        advance(60).await;
        assert_eq!(single_state(&rig.last(), 0), SlotState::Stale(out("one")));
        advance(61).await;
        assert_eq!(single_state(&rig.last(), 0), SlotState::Ok(out("two")));
    }

    #[tokio::test(start_paused = true)]
    async fn first_failure_without_output_is_error() {
        let rig = Rig::start(data(vec![inst(1, false)]), |_| Err(RunError::Timeout));
        advance(3).await;
        assert!(matches!(
            single_state(&rig.last(), 0),
            SlotState::Error { last: None, .. }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn one_failure_does_not_stop_others() {
        let b_runs = Arc::new(AtomicUsize::new(0));
        let b = b_runs.clone();
        let (a_id, b_id) = (id(1), id(2));
        let rig = Rig::start(data(vec![inst(1, false), inst(2, false)]), move |who| {
            if who == a_id {
                Err(RunError::Script("boom".into()))
            } else {
                assert_eq!(who, b_id);
                Ok(out(&b.fetch_add(1, Ordering::SeqCst).to_string()))
            }
        });
        advance(3).await;
        for n in 0..3 {
            let m = rig.last();
            assert!(matches!(
                single_state(&m, 0),
                SlotState::Error { last: None, .. }
            ));
            assert_eq!(single_state(&m, 1), SlotState::Ok(out(&n.to_string())));
            advance(60).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hidden_instances_not_published_and_not_run() {
        let hidden_runs = Arc::new(AtomicUsize::new(0));
        let h = hidden_runs.clone();
        let hid = id(2);
        let rig = Rig::start(data(vec![inst(1, false), inst(2, true)]), move |who| {
            if who == hid {
                h.fetch_add(1, Ordering::SeqCst);
            }
            Ok(out("x"))
        });
        advance(130).await;
        assert_eq!(hidden_runs.load(Ordering::SeqCst), 0);
        assert_eq!(rig.last().slots.len(), 1);
        assert_eq!(rig.last().slots[0].id, id(1));

        rig.handle.set_hidden(&id(2), false);
        assert_eq!(rig.last().slots.len(), 2);
        assert_eq!(single_state(&rig.last(), 1), SlotState::Loading);
        advance(3).await;
        assert_eq!(single_state(&rig.last(), 1), SlotState::Ok(out("x")));
        assert_eq!(hidden_runs.load(Ordering::SeqCst), 1);

        rig.handle.set_hidden(&id(1), true);
        assert_eq!(rig.last().slots.len(), 1);
        let saved = crate::instances::load(&rig.path);
        assert!(
            saved
                .instances
                .iter()
                .find(|i| i.id == id(1))
                .unwrap()
                .hidden
        );
        assert!(
            !saved
                .instances
                .iter()
                .find(|i| i.id == id(2))
                .unwrap()
                .hidden
        );
    }

    #[tokio::test(start_paused = true)]
    async fn group_members_published_in_group_slot() {
        let d = Instances {
            instances: vec![inst(1, false), inst(2, false), inst(3, false)],
            groups: vec![Group {
                id: "grp-00000001".into(),
                members: vec![id(1), id(2)],
                interval: 8,
            }],
            order: vec![id(3), "grp-00000001".into()],
        };
        let rig = Rig::start(d, |who| Ok(out(who)));
        advance(3).await;
        let m = rig.last();
        assert_eq!(m.slots.len(), 2);
        assert_eq!(m.slots[0].id, id(3));
        assert_eq!(m.slots[1].id, "grp-00000001");
        match &m.slots[1].kind {
            SlotKind::Group { interval, members } => {
                assert_eq!(*interval, 8);
                let got: Vec<&Member> = members.iter().collect();
                assert_eq!(got.len(), 2);
                assert_eq!(got[0].instance_id, id(1));
                assert_eq!(got[0].state, SlotState::Ok(out(&id(1))));
                assert_eq!(*got[0].widget_dir, PathBuf::from("wdir"));
                assert_eq!(got[1].instance_id, id(2));
                assert_eq!(got[1].state, SlotState::Ok(out(&id(2))));
            }
            other => panic!("not a group: {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn publish_only_on_change() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let rig = Rig::start(data(vec![inst(1, false)]), move |_| {
            r.fetch_add(1, Ordering::SeqCst);
            Ok(out("same"))
        });
        advance(200).await;
        assert!(runs.load(Ordering::SeqCst) >= 3);
        // Initial Loading model, then one change to Ok; repeats are silent.
        assert_eq!(rig.count(), 2);
        assert_eq!(rig.last().generation, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn refresh_now_runs_without_waiting() {
        let runs = Arc::new(AtomicUsize::new(0));
        let r = runs.clone();
        let rig = Rig::start(data(vec![inst(1, false)]), move |_| {
            Ok(out(&r.fetch_add(1, Ordering::SeqCst).to_string()))
        });
        advance(3).await;
        assert_eq!(single_state(&rig.last(), 0), SlotState::Ok(out("0")));
        rig.handle.refresh_now(&id(1));
        advance(1).await;
        assert_eq!(single_state(&rig.last(), 0), SlotState::Ok(out("1")));
    }

    #[tokio::test(start_paused = true)]
    async fn runs_of_one_instance_never_overlap() {
        let active = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));
        let (a, mx) = (active.clone(), max_seen.clone());
        let dir = tempfile::tempdir().unwrap();
        let widgets = HashMap::from([("w".to_string(), (manifest(), PathBuf::from("wdir")))]);
        let runner: Runner = Arc::new(move |_ctx| {
            let (a, mx) = (a.clone(), mx.clone());
            Box::pin(async move {
                let now = a.fetch_add(1, Ordering::SeqCst) + 1;
                mx.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(5)).await;
                a.fetch_sub(1, Ordering::SeqCst);
                Ok(out("x"))
            })
        });
        let handle = Scheduler::start_with_runner(
            widgets,
            data(vec![inst(1, false)]),
            Arc::new(NoHttp),
            runner,
            dir.path().join("i.json"),
            |_| {},
        );
        advance(3).await; // a run is in flight
        for _ in 0..5 {
            handle.refresh_now(&id(1));
        }
        advance(60).await;
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }
}
