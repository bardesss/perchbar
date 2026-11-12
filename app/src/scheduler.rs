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
//! after the state lock is released (calls are serialised, in generation
//! order), so handle methods are safe to call from other threads while it
//! runs. It must still not call back into the `SchedulerHandle` itself.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
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
/// How long `shutdown` waits for a pending instances.json write.
const SHUTDOWN_SAVE_WAIT: Duration = Duration::from_secs(1);

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

/// Writes `instances.json`. Injectable so tests can make the disk slow.
pub type Writer = Arc<dyn Fn(&Path, &Instances) -> std::io::Result<()> + Send + Sync>;

#[derive(Default)]
struct SaverState {
    /// The newest snapshot not yet written.
    latest: Option<Instances>,
    writing: bool,
    closed: bool,
}

/// Persists `instances.json` on its own thread, so callers (the strip thread
/// among them) never wait for the disk. Snapshots are submitted in the order
/// the state changed; the thread always writes the newest one and skips any
/// it was too slow to write, so the file ends up matching the last change.
///
/// If the thread cannot start, `submit` writes synchronously instead: disk
/// I/O on the caller is the lesser evil next to losing changes.
struct Saver {
    state: Arc<(Mutex<SaverState>, Condvar)>,
    /// Set only when the thread failed to start.
    fallback: Option<(PathBuf, Writer)>,
}

/// Starts a job on a new thread. Injectable so tests can make it fail.
type SpawnJob = Box<dyn FnOnce() + Send>;

impl Saver {
    fn start(path: PathBuf, writer: Writer) -> Saver {
        Self::start_with(path, writer, |job: SpawnJob| {
            std::thread::Builder::new()
                .name("perchbar-saver".into())
                .spawn(job)
                .map(|_| ())
        })
    }

    fn start_with(
        path: PathBuf,
        writer: Writer,
        spawn: impl FnOnce(SpawnJob) -> std::io::Result<()>,
    ) -> Saver {
        let state: Arc<(Mutex<SaverState>, Condvar)> = Arc::default();
        let shared = Arc::clone(&state);
        let (thread_path, thread_writer) = (path.clone(), writer.clone());
        let spawned = spawn(Box::new(move || {
            let (path, writer) = (thread_path, thread_writer);
            {
                let (lock, cv) = &*shared;
                let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
                loop {
                    if let Some(snapshot) = s.latest.take() {
                        s.writing = true;
                        drop(s);
                        if let Err(e) = writer(&path, &snapshot) {
                            log::warn!("cannot save instances: {e}");
                        }
                        s = lock.lock().unwrap_or_else(|p| p.into_inner());
                        s.writing = false;
                        cv.notify_all();
                    } else if s.closed {
                        return;
                    } else {
                        s = cv.wait(s).unwrap_or_else(|p| p.into_inner());
                    }
                }
            }
        }));
        let fallback = match spawned {
            Ok(()) => None,
            Err(e) => {
                log::warn!("cannot start the instances saver ({e}); saving synchronously");
                Some((path, writer))
            }
        };
        Saver { state, fallback }
    }

    /// Queues a snapshot. Never blocks on the disk, except in the fallback
    /// mode where there is no saver thread.
    fn submit(&self, snapshot: Instances) {
        if let Some((path, writer)) = &self.fallback {
            if let Err(e) = writer(path, &snapshot) {
                log::warn!("cannot save instances: {e}");
            }
            return;
        }
        let (lock, cv) = &*self.state;
        lock.lock().unwrap_or_else(|p| p.into_inner()).latest = Some(snapshot);
        cv.notify_all();
    }

    /// Waits up to `timeout` for queued snapshots to reach the disk. Returns
    /// whether everything was written.
    fn flush(&self, timeout: Duration) -> bool {
        let (lock, cv) = &*self.state;
        let deadline = std::time::Instant::now() + timeout;
        let mut s = lock.lock().unwrap_or_else(|p| p.into_inner());
        while s.latest.is_some() || s.writing {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            s = cv
                .wait_timeout(s, left)
                .unwrap_or_else(|p| p.into_inner())
                .0;
        }
        true
    }
}

impl Drop for Saver {
    fn drop(&mut self) {
        let (lock, cv) = &*self.state;
        lock.lock().unwrap_or_else(|p| p.into_inner()).closed = true;
        cv.notify_all();
    }
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
    /// Per-instance run guards. They outlive the loop task, so a run that is
    /// still executing after hide/show blocks the next loop from starting another.
    guards: HashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

struct Shared {
    /// The runtime the scheduler started in. Loops are spawned through it,
    /// so handle methods work from threads outside the runtime (the strip).
    runtime: tokio::runtime::Handle,
    widgets: HashMap<String, (Arc<Manifest>, Arc<PathBuf>)>,
    http: Arc<dyn HttpClient>,
    runner: Runner,
    semaphore: Arc<Semaphore>,
    publish: Box<dyn Fn(StripModel) + Send + Sync>,
    /// Serialises publish calls so models reach the callback in generation order.
    publish_lock: Mutex<()>,
    /// Writes instances.json in the background.
    saver: Saver,
    inner: Mutex<Inner>,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Runs `f` on the state, then rebuilds the slots. If they changed, bumps
    /// the generation and publishes. The state lock is released before the
    /// callback runs; the publish lock is taken before that release, so
    /// callbacks still arrive in generation order.
    fn update<R>(&self, f: impl FnOnce(&mut Inner) -> R) -> R {
        let mut inner = self.lock();
        let result = f(&mut inner);
        let slots = self.build_slots(&inner);
        if slots != inner.model.slots {
            inner.model.slots = slots;
            inner.model.generation += 1;
            let model = inner.model.clone();
            let order = self.publish_lock.lock().unwrap_or_else(|p| p.into_inner());
            drop(inner);
            (self.publish)(model);
            drop(order);
        }
        result
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
        Self::start_with_writer(
            widgets,
            instances,
            http,
            runner,
            instances_path,
            Arc::new(|p: &Path, d: &Instances| instances::save(p, d)),
            publish,
        )
    }

    pub fn start_with_writer(
        widgets: HashMap<String, (Arc<Manifest>, PathBuf)>,
        instances: Instances,
        http: Arc<dyn HttpClient>,
        runner: Runner,
        instances_path: PathBuf,
        writer: Writer,
        publish: impl Fn(StripModel) + Send + Sync + 'static,
    ) -> SchedulerHandle {
        let shared = Arc::new(Shared {
            runtime: tokio::runtime::Handle::current(),
            widgets: widgets
                .into_iter()
                .map(|(id, (m, dir))| (id, (m, Arc::new(dir))))
                .collect(),
            http,
            runner,
            semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_RUNS)),
            publish: Box::new(publish),
            publish_lock: Mutex::new(()),
            saver: Saver::start(instances_path, writer),
            inner: Mutex::new(Inner {
                data: instances,
                states: HashMap::new(),
                model: StripModel::default(),
                tasks: HashMap::new(),
                guards: HashMap::new(),
            }),
        });
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
        let model = inner.model.clone();
        let order = shared
            .publish_lock
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        drop(inner);
        (shared.publish)(model);
        drop(order);
        SchedulerHandle { shared }
    }
}

fn spawn_instance(shared: &Arc<Shared>, inner: &mut Inner, id: String) {
    inner.states.insert(id.clone(), SlotState::Loading);
    let notify = Arc::new(Notify::new());
    let guard = inner.guards.entry(id.clone()).or_default().clone();
    let task = shared.runtime.spawn(instance_loop(
        shared.clone(),
        id.clone(),
        notify.clone(),
        guard,
    ));
    inner.tasks.insert(id, Running { notify, task });
}

async fn instance_loop(
    shared: Arc<Shared>,
    id: String,
    notify: Arc<Notify>,
    guard: Arc<tokio::sync::Mutex<()>>,
) {
    let mut wait = Duration::from_millis(rand::random_range(0..=MAX_START_JITTER_MS));
    let mut failures = 0u32;
    let mut last_good: Option<Output> = None;
    loop {
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = notify.notified() => {}
        }
        // A run left over from before a hide/show may still be executing; its
        // task holds the guard, so wait for it before starting another.
        let run_guard = guard.clone().lock_owned().await;
        let Some((ctx, refresh)) = make_context(&shared, &id) else {
            return;
        };
        let Ok(permit) = shared.semaphore.clone().acquire_owned().await else {
            return;
        };
        // The run lives in its own task that owns the guard and the permit.
        // Aborting this loop (hide, shutdown) only drops the join handle, so
        // the permit and guard are released when the run itself ends, never
        // earlier. The real runner's blocking work is covered by this too.
        let fut = (shared.runner)(ctx);
        let result = tokio::spawn(async move {
            let _held = (run_guard, permit);
            fut.await
        })
        .await
        .unwrap_or_else(|e| Err(RunError::Script(format!("run aborted: {e}"))));
        let state = match result {
            Ok(output) => {
                log::debug!("widget run ok for {id}");
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
        shared.update(|inner| {
            // Skip if the instance was hidden or removed while the run was in flight.
            if let Some(slot) = inner.states.get_mut(&id) {
                *slot = state;
            }
        });
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
    /// Hidden instances are not run; showing one starts it as Loading. A run
    /// still executing from before a hide finishes first (its result is
    /// discarded); the restarted loop waits for it, so runs never overlap.
    pub fn set_hidden(&self, instance_id: &str, hidden: bool) {
        let shared = &self.shared;
        let changed = shared.update(|inner| {
            let Some(inst) = inner
                .data
                .instances
                .iter_mut()
                .find(|i| i.id == instance_id)
            else {
                return false;
            };
            if inst.hidden == hidden {
                return false;
            }
            inst.hidden = hidden;
            let widget_known = shared.widgets.contains_key(&inst.widget);
            if hidden {
                inner.states.remove(instance_id);
                if let Some(running) = inner.tasks.remove(instance_id) {
                    running.task.abort();
                }
            } else if widget_known && !inner.tasks.contains_key(instance_id) {
                spawn_instance(shared, inner, instance_id.to_string());
            }
            // Submitted under the state lock, so snapshots reach the saver in
            // the order the changes happened. The saver writes off-thread.
            shared.saver.submit(inner.data.clone());
            true
        });
        if changed {
            log::debug!("{instance_id} hidden: {hidden}; saving in the background");
        }
    }

    /// Waits up to `timeout` for pending `instances.json` writes. Returns
    /// whether everything reached the disk.
    pub fn flush_saves(&self, timeout: Duration) -> bool {
        self.shared.saver.flush(timeout)
    }

    /// Stops all instance loops. A run already executing finishes on its own
    /// and its result is discarded. Waits briefly for a pending save, so a
    /// change made just before quitting is not lost.
    pub fn shutdown(self) {
        {
            let mut inner = self.shared.lock();
            for (_, running) in inner.tasks.drain() {
                running.task.abort();
            }
        }
        if !self.flush_saves(SHUTDOWN_SAVE_WAIT) {
            log::warn!("instances.json was not saved within {SHUTDOWN_SAVE_WAIT:?}");
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
        assert!(rig.handle.flush_saves(Duration::from_secs(5)));
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
    async fn unknown_widget_instance_is_not_shown_or_run_but_is_saved() {
        let runs = Arc::new(Mutex::new(Vec::<String>::new()));
        let r = runs.clone();
        let mut orphan = inst(2, false);
        orphan.widget = "gone".into();
        let rig = Rig::start(data(vec![inst(1, false), orphan]), move |who| {
            r.lock().unwrap().push(who.to_string());
            Ok(out("x"))
        });
        advance(3).await;
        assert_eq!(rig.last().slots.len(), 1);
        assert_eq!(rig.last().slots[0].id, id(1));
        assert!(runs.lock().unwrap().iter().all(|w| *w == id(1)));
        rig.handle.set_hidden(&id(1), true);
        assert!(rig.handle.flush_saves(Duration::from_secs(5)));
        let saved = crate::instances::load(&rig.path);
        assert!(saved.instances.iter().any(|i| i.id == id(2) && i.widget == "gone"));
    }

    #[test]
    fn showing_an_instance_from_a_plain_thread_does_not_panic() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let published: Published = Arc::default();
        let sink = published.clone();
        let handle = rt.block_on(async {
            Scheduler::start_with_runner(
                HashMap::from([("w".to_string(), (manifest(), PathBuf::from("wdir")))]),
                data(vec![inst(1, true)]),
                Arc::new(NoHttp),
                Arc::new(|_| Box::pin(async { Ok(out("x")) })),
                dir.path().join("i.json"),
                move |m| sink.lock().unwrap().push(m),
            )
        });
        // No runtime context on this thread, like the strip thread.
        let handle = std::thread::spawn(move || {
            handle.set_hidden(&id(1), false);
            handle
        })
        .join()
        .expect("set_hidden panicked off the runtime");
        let last = published.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.slots.len(), 1);
        handle.shutdown();
        rt.shutdown_background();
    }

    #[test]
    fn saver_writes_synchronously_when_its_thread_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        let writes = Arc::new(AtomicUsize::new(0));
        let w = writes.clone();
        let writer: Writer = Arc::new(move |p: &Path, d: &Instances| {
            w.fetch_add(1, Ordering::SeqCst);
            instances::save(p, d)
        });
        let saver = Saver::start_with(path.clone(), writer, |_job| {
            Err(std::io::Error::other("no threads"))
        });
        saver.submit(data(vec![inst(1, true)]));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(instances::load(&path).instances[0].hidden);
        saver.submit(data(vec![inst(1, false)]));
        assert!(!instances::load(&path).instances[0].hidden);
        // Nothing is pending, so a flush (and shutdown) returns at once.
        let t0 = std::time::Instant::now();
        assert!(saver.flush(Duration::from_secs(5)));
        assert!(t0.elapsed() < Duration::from_millis(500));
    }

    #[test]
    fn set_hidden_does_not_wait_for_disk_and_the_last_call_wins() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("instances.json");
        // A writer that blocks until the test opens the gate.
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let writes = Arc::new(AtomicUsize::new(0));
        let (g, w) = (gate.clone(), writes.clone());
        let writer: Writer = Arc::new(move |p: &Path, d: &Instances| {
            let (lock, cv) = &*g;
            let mut open = lock.lock().unwrap();
            while !*open {
                open = cv.wait(open).unwrap();
            }
            w.fetch_add(1, Ordering::SeqCst);
            instances::save(p, d)
        });
        let published: Published = Arc::default();
        let sink = published.clone();
        let handle = rt.block_on(async {
            Scheduler::start_with_writer(
                HashMap::from([("w".to_string(), (manifest(), PathBuf::from("wdir")))]),
                data(vec![inst(1, false), inst(2, false)]),
                Arc::new(NoHttp),
                Arc::new(|_| Box::pin(std::future::pending())),
                path.clone(),
                writer,
                move |m| sink.lock().unwrap().push(m),
            )
        });

        // Showing an instance spawns its loop, which needs the runtime.
        let _rt = rt.enter();
        // The writer is stuck, yet every call returns and publishes at once.
        let t0 = std::time::Instant::now();
        handle.set_hidden(&id(1), true);
        handle.set_hidden(&id(2), true);
        handle.set_hidden(&id(1), false);
        assert!(t0.elapsed() < Duration::from_secs(1));
        let last = published.lock().unwrap().last().cloned().unwrap();
        assert_eq!(last.slots.len(), 1);
        assert_eq!(last.slots[0].id, id(1));
        assert!(!path.exists());

        {
            let (lock, cv) = &*gate;
            *lock.lock().unwrap() = true;
            cv.notify_all();
        }
        assert!(handle.flush_saves(Duration::from_secs(5)));
        let saved = instances::load(&path);
        let hidden = |n: u32| {
            saved
                .instances
                .iter()
                .find(|i| i.id == id(n))
                .unwrap()
                .hidden
        };
        assert!(!hidden(1));
        assert!(hidden(2));
        // Three changes, coalesced into at most two writes.
        assert!(writes.load(Ordering::SeqCst) <= 2);
        handle.shutdown();
        rt.shutdown_background();
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

    /// Tracks how many runs execute at once, per instance and overall.
    #[derive(Default)]
    struct Concurrency {
        per_instance: Mutex<HashMap<String, usize>>,
        active: AtomicUsize,
        max_total: AtomicUsize,
        max_per_instance: AtomicUsize,
        runs: AtomicUsize,
    }

    fn slow_runner(c: Arc<Concurrency>) -> Runner {
        Arc::new(move |ctx: RunContext| {
            let c = c.clone();
            Box::pin(async move {
                c.runs.fetch_add(1, Ordering::SeqCst);
                let total = c.active.fetch_add(1, Ordering::SeqCst) + 1;
                c.max_total.fetch_max(total, Ordering::SeqCst);
                let mine = {
                    let mut m = c.per_instance.lock().unwrap();
                    let n = m.entry(ctx.instance_id.clone()).or_default();
                    *n += 1;
                    *n
                };
                c.max_per_instance.fetch_max(mine, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_secs(5)).await;
                *c.per_instance
                    .lock()
                    .unwrap()
                    .get_mut(&ctx.instance_id)
                    .unwrap() -= 1;
                c.active.fetch_sub(1, Ordering::SeqCst);
                Ok(out("x"))
            })
        })
    }

    fn start_slow(count: u32, c: Arc<Concurrency>) -> (SchedulerHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let widgets = HashMap::from([("w".to_string(), (manifest(), PathBuf::from("wdir")))]);
        let handle = Scheduler::start_with_runner(
            widgets,
            data((1..=count).map(|n| inst(n, false)).collect()),
            Arc::new(NoHttp),
            slow_runner(c),
            dir.path().join("i.json"),
            |_| {},
        );
        (handle, dir)
    }

    #[tokio::test(start_paused = true)]
    async fn hide_then_show_mid_run_does_not_overlap() {
        let c = Arc::new(Concurrency::default());
        let (handle, _dir) = start_slow(1, c.clone());
        advance(3).await; // the first run is in flight
        assert_eq!(c.active.load(Ordering::SeqCst), 1);
        handle.set_hidden(&id(1), true);
        handle.set_hidden(&id(1), false);
        advance(120).await;
        assert_eq!(c.max_per_instance.load(Ordering::SeqCst), 1);
        // The restarted loop did run again after the first run ended.
        assert!(c.runs.load(Ordering::SeqCst) >= 2);
    }

    #[tokio::test(start_paused = true)]
    async fn global_run_cap_holds_across_hide_show_toggles() {
        let c = Arc::new(Concurrency::default());
        let (handle, _dir) = start_slow(10, c.clone());
        for round in 0..4 {
            advance(3).await;
            for n in 1..=10 {
                handle.set_hidden(&id(n), true);
                handle.set_hidden(&id(n), false);
            }
            advance(round + 1).await;
        }
        advance(120).await;
        assert_eq!(c.max_per_instance.load(Ordering::SeqCst), 1);
        assert_eq!(c.max_total.load(Ordering::SeqCst), MAX_CONCURRENT_RUNS);
    }
}
