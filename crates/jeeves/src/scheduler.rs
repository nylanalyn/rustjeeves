//! Host-owned durable scheduler for WASM modules.

use crate::db::DbHandle;
use crate::log_bus::LogBus;
use anyhow::{anyhow, bail, Result};
use jeeves_abi::{Event, EventEnvelope, ScheduleSet, ScheduledJob};
use std::collections::{HashMap, HashSet};
use std::sync::{mpsc as std_mpsc, Arc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot};

pub const MAX_JOBS_PER_MODULE: usize = 128;
pub const MAX_PAYLOAD_BYTES: usize = 2048;
pub const MAX_ID_BYTES: usize = 160;
pub const MAX_HORIZON_SECONDS: i64 = 366 * 24 * 60 * 60;
const ABSENT_RETRY_SECONDS: i64 = 30;

pub(crate) struct ScheduledDelivery {
    pub module: String,
    pub envelope: EventEnvelope,
    pub completion: ScheduledCompletion,
}

pub(crate) struct ScheduledCompletion {
    scheduler: std_mpsc::SyncSender<Request>,
    job: ScheduledJob,
}

impl ScheduledCompletion {
    pub fn finish(self, succeeded: bool) {
        let _ = self.scheduler.send(Request::DeliveryComplete {
            job: Box::new(self.job),
            succeeded,
        });
    }
}

enum Request {
    Set {
        module: String,
        request: ScheduleSet,
        reply: oneshot::Sender<Result<()>>,
    },
    Cancel {
        module: String,
        id: String,
        reply: oneshot::Sender<Result<bool>>,
    },
    List {
        module: String,
        server: Option<String>,
        channel: Option<String>,
        reply: oneshot::Sender<Result<Vec<ScheduledJob>>>,
    },
    ListAll {
        reply: oneshot::Sender<Result<Vec<ScheduledJob>>>,
    },
    DeliveryComplete {
        job: Box<ScheduledJob>,
        succeeded: bool,
    },
    Shutdown,
}

#[derive(Clone)]
pub struct SchedulerHandle {
    inner: Arc<SchedulerSender>,
}

struct SchedulerSender {
    tx: std_mpsc::SyncSender<Request>,
}

impl Drop for SchedulerSender {
    fn drop(&mut self) {
        let _ = self.tx.send(Request::Shutdown);
    }
}

impl SchedulerHandle {
    pub fn spawn(db: DbHandle, deliveries: mpsc::Sender<ScheduledDelivery>, log: LogBus) -> Self {
        let (tx, rx) = std_mpsc::sync_channel(256);
        let completion_tx = tx.clone();
        std::thread::Builder::new()
            .name("jeeves-scheduler".into())
            .spawn(move || run(db, deliveries, completion_tx, log, rx))
            .expect("spawn scheduler thread");
        Self {
            inner: Arc::new(SchedulerSender { tx }),
        }
    }

    pub fn set_blocking(&self, module: &str, request: ScheduleSet) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(Request::Set {
                module: module.into(),
                request,
                reply,
            })
            .map_err(|_| anyhow!("scheduler is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("scheduler dropped reply"))?
    }

    pub fn cancel_blocking(&self, module: &str, id: &str) -> Result<bool> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(Request::Cancel {
                module: module.into(),
                id: id.into(),
                reply,
            })
            .map_err(|_| anyhow!("scheduler is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("scheduler dropped reply"))?
    }

    pub fn list_blocking(
        &self,
        module: &str,
        server: Option<&str>,
        channel: Option<&str>,
    ) -> Result<Vec<ScheduledJob>> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(Request::List {
                module: module.into(),
                server: server.map(str::to_string),
                channel: channel.map(str::to_string),
                reply,
            })
            .map_err(|_| anyhow!("scheduler is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("scheduler dropped reply"))?
    }

    pub fn list_all_blocking(&self) -> Result<Vec<ScheduledJob>> {
        let (reply, rx) = oneshot::channel();
        self.inner
            .tx
            .try_send(Request::ListAll { reply })
            .map_err(|_| anyhow!("scheduler is gone"))?;
        rx.blocking_recv()
            .map_err(|_| anyhow!("scheduler dropped reply"))?
    }
}

fn run(
    db: DbHandle,
    deliveries: mpsc::Sender<ScheduledDelivery>,
    completion_tx: std_mpsc::SyncSender<Request>,
    log: LogBus,
    rx: std_mpsc::Receiver<Request>,
) {
    let mut jobs: HashMap<(String, String), ScheduledJob> = db
        .scheduled_jobs_load_blocking()
        .unwrap_or_else(|error| {
            log.error("scheduler", format!("cannot restore jobs: {error}"));
            Vec::new()
        })
        .into_iter()
        .map(|job| ((job.module.clone(), job.id.clone()), job))
        .collect();
    let mut retry_after = HashMap::<(String, String), i64>::new();
    let mut in_flight = HashSet::<(String, String)>::new();
    let mut completed = HashSet::<(String, String)>::new();

    loop {
        finish_completed(&db, &log, &mut jobs, &mut retry_after, &mut completed);
        deliver_due(
            &deliveries,
            &completion_tx,
            &log,
            &jobs,
            &mut retry_after,
            &mut in_flight,
            &completed,
        );
        let now = now_secs();
        let wait = jobs
            .iter()
            .filter(|(key, _)| !in_flight.contains(*key))
            .map(|(key, job)| retry_after.get(key).copied().unwrap_or(job.due_at))
            .min()
            .map(|due| Duration::from_secs(due.saturating_sub(now).clamp(0, 60) as u64))
            .unwrap_or(Duration::from_secs(60));
        match rx.recv_timeout(wait) {
            Ok(Request::Shutdown) => break,
            Ok(request) => handle_request(
                &db,
                &log,
                &mut jobs,
                &mut retry_after,
                &mut in_flight,
                &mut completed,
                request,
            ),
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_request(
    db: &DbHandle,
    log: &LogBus,
    jobs: &mut HashMap<(String, String), ScheduledJob>,
    retry_after: &mut HashMap<(String, String), i64>,
    in_flight: &mut HashSet<(String, String)>,
    completed: &mut HashSet<(String, String)>,
    request: Request,
) {
    match request {
        Request::Set {
            module,
            request,
            reply,
        } => {
            let result = validate_set(&module, &request, jobs).and_then(|()| {
                let job = ScheduledJob {
                    module: module.clone(),
                    id: request.id,
                    server: request.server,
                    channel: request.channel,
                    owner_profile_id: request.owner_profile_id,
                    due_at: request.due_at,
                    payload: request.payload,
                    created_at: now_secs(),
                };
                db.scheduled_job_set_blocking(job.clone())?;
                let key = (module.clone(), job.id.clone());
                retry_after.remove(&key);
                completed.remove(&key);
                jobs.insert(key, job.clone());
                let delta = job.due_at.saturating_sub(now_secs());
                log.info(
                    "scheduler",
                    format!(
                        "{module}: scheduled job '{}' for {}:{} in {delta}s (due_at={})",
                        job.id, job.server, job.channel, job.due_at
                    ),
                );
                Ok(())
            });
            let _ = reply.send(result);
        }
        Request::Cancel { module, id, reply } => {
            let result = match db.scheduled_job_delete_blocking(&module, &id) {
                Ok(removed) => {
                    let key = (module.clone(), id);
                    jobs.remove(&key);
                    retry_after.remove(&key);
                    in_flight.remove(&key);
                    completed.remove(&key);
                    if removed {
                        log.info("scheduler", format!("{module}: cancelled job '{}'", key.1));
                    }
                    Ok(removed)
                }
                Err(error) => Err(error),
            };
            let _ = reply.send(result);
        }
        Request::List {
            module,
            server,
            channel,
            reply,
        } => {
            let mut found = jobs
                .values()
                .filter(|job| job.module == module)
                .filter(|job| server.as_ref().is_none_or(|value| job.server == *value))
                .filter(|job| channel.as_ref().is_none_or(|value| job.channel == *value))
                .cloned()
                .collect::<Vec<_>>();
            found.sort_by_key(|job| (job.due_at, job.id.clone()));
            let _ = reply.send(Ok(found));
        }
        Request::ListAll { reply } => {
            let mut found = jobs.values().cloned().collect::<Vec<_>>();
            found.sort_by_key(|job| (job.due_at, job.module.clone(), job.id.clone()));
            let _ = reply.send(Ok(found));
        }
        Request::DeliveryComplete { job, succeeded } => {
            let key = (job.module.clone(), job.id.clone());
            in_flight.remove(&key);

            // A module may replace or cancel the same ID while handling its timer. An old
            // completion must never delete that newer job.
            if jobs.get(&key) != Some(job.as_ref()) {
                return;
            }

            if succeeded {
                match db.scheduled_job_delete_blocking(&job.module, &job.id) {
                    Ok(_) => {
                        jobs.remove(&key);
                        retry_after.remove(&key);
                        let overdue = now_secs().saturating_sub(job.due_at);
                        let overdue_note = if overdue > 0 {
                            format!(" ({overdue}s overdue)")
                        } else {
                            String::new()
                        };
                        log.info(
                            "scheduler",
                            format!(
                                "{}: completed job '{}' for {}:{}{}",
                                job.module, job.id, job.server, job.channel, overdue_note
                            ),
                        );
                    }
                    Err(error) => {
                        log.error(
                            "scheduler",
                            format!("cannot complete job '{}': {error}", job.id),
                        );
                        completed.insert(key.clone());
                        retry_after.insert(key, now_secs().saturating_add(ABSENT_RETRY_SECONDS));
                    }
                }
            } else {
                log.info(
                    "scheduler",
                    format!(
                        "{}: module rejected or failed job '{}'; retry in {ABSENT_RETRY_SECONDS}s",
                        job.module, job.id
                    ),
                );
                retry_after.insert(key, now_secs().saturating_add(ABSENT_RETRY_SECONDS));
            }
        }
        Request::Shutdown => unreachable!("shutdown is handled by the scheduler loop"),
    }
}

fn deliver_due(
    deliveries: &mpsc::Sender<ScheduledDelivery>,
    completion_tx: &std_mpsc::SyncSender<Request>,
    log: &LogBus,
    jobs: &HashMap<(String, String), ScheduledJob>,
    retry_after: &mut HashMap<(String, String), i64>,
    in_flight: &mut HashSet<(String, String)>,
    completed: &HashSet<(String, String)>,
) {
    let now = now_secs();
    let mut due = jobs
        .iter()
        .filter(|(key, job)| {
            !in_flight.contains(*key)
                && !completed.contains(*key)
                && job.due_at <= now
                && retry_after.get(*key).is_none_or(|retry| *retry <= now)
        })
        .map(|(key, job)| (key.clone(), job.clone()))
        .collect::<Vec<_>>();
    due.sort_by_key(|(_, job)| (job.due_at, job.created_at));
    for (key, job) in due {
        let delivery = ScheduledDelivery {
            module: job.module.clone(),
            envelope: EventEnvelope {
                server: job.server.clone(),
                event: Event::Timer {
                    id: job.id.clone(),
                    channel: job.channel.clone(),
                    due_at: job.due_at,
                    payload: job.payload.clone(),
                },
            },
            completion: ScheduledCompletion {
                scheduler: completion_tx.clone(),
                job: job.clone(),
            },
        };
        if deliveries.try_send(delivery).is_ok() {
            in_flight.insert(key);
        } else {
            log.info(
                "scheduler",
                format!(
                    "{}: delivery queue unavailable for job '{}' at {}:{}; retry in {ABSENT_RETRY_SECONDS}s",
                    job.module, job.id, job.server, job.channel
                ),
            );
            retry_after.insert(key, now.saturating_add(ABSENT_RETRY_SECONDS));
        }
    }
}

fn finish_completed(
    db: &DbHandle,
    log: &LogBus,
    jobs: &mut HashMap<(String, String), ScheduledJob>,
    retry_after: &mut HashMap<(String, String), i64>,
    completed: &mut HashSet<(String, String)>,
) {
    let now = now_secs();
    let ready = completed
        .iter()
        .filter(|key| retry_after.get(*key).is_none_or(|retry| *retry <= now))
        .cloned()
        .collect::<Vec<_>>();
    for key in ready {
        let Some(job) = jobs.get(&key).cloned() else {
            completed.remove(&key);
            retry_after.remove(&key);
            continue;
        };
        match db.scheduled_job_delete_blocking(&job.module, &job.id) {
            Ok(_) => {
                jobs.remove(&key);
                completed.remove(&key);
                retry_after.remove(&key);
                log.info(
                    "scheduler",
                    format!(
                        "{}: completed deferred cleanup for job '{}'",
                        job.module, job.id
                    ),
                );
            }
            Err(error) => {
                log.error(
                    "scheduler",
                    format!("cannot clean up completed job '{}': {error}", job.id),
                );
                retry_after.insert(key, now.saturating_add(ABSENT_RETRY_SECONDS));
            }
        }
    }
}

fn validate_set(
    module: &str,
    request: &ScheduleSet,
    jobs: &HashMap<(String, String), ScheduledJob>,
) -> Result<()> {
    if request.id.is_empty() || request.id.len() > MAX_ID_BYTES {
        bail!("job id must contain 1–{MAX_ID_BYTES} bytes");
    }
    if request.server.trim().is_empty() || request.channel.trim().is_empty() {
        bail!("server and channel are required");
    }
    if request.payload.len() > MAX_PAYLOAD_BYTES {
        bail!("job payload exceeds {MAX_PAYLOAD_BYTES} bytes");
    }
    let now = now_secs();
    if request.due_at <= now || request.due_at > now.saturating_add(MAX_HORIZON_SECONDS) {
        bail!("due time must be in the future within {MAX_HORIZON_SECONDS} seconds");
    }
    let replacing = jobs.contains_key(&(module.to_string(), request.id.clone()));
    let count = jobs.values().filter(|job| job.module == module).count();
    if !replacing && count >= MAX_JOBS_PER_MODULE {
        bail!("module job quota of {MAX_JOBS_PER_MODULE} reached");
    }
    Ok(())
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persists_delivers_and_completes_a_job() {
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(16);
        let (deliveries, mut rx) = mpsc::channel(4);
        let scheduler = SchedulerHandle::spawn(db.clone(), deliveries, log);
        scheduler
            .set_blocking(
                "reminders",
                ScheduleSet {
                    id: "alice:1".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 1,
                    payload: "payload".into(),
                },
            )
            .unwrap();
        assert_eq!(
            scheduler
                .list_blocking("reminders", Some("net"), Some("#room"))
                .unwrap()
                .len(),
            1
        );

        let delivery = rx.blocking_recv().expect("scheduled delivery");
        assert_eq!(delivery.module, "reminders");
        assert!(matches!(
            delivery.envelope.event,
            Event::Timer { ref id, .. } if id == "alice:1"
        ));
        delivery.completion.finish(true);

        for _ in 0..20 {
            if scheduler
                .list_blocking("reminders", None, None)
                .unwrap()
                .is_empty()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("delivered job was not completed");
    }

    #[test]
    fn cancellation_is_namespaced_and_idempotent() {
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(16);
        let (deliveries, _rx) = mpsc::channel(1);
        let scheduler = SchedulerHandle::spawn(db, deliveries, log);
        scheduler
            .set_blocking(
                "reminders",
                ScheduleSet {
                    id: "one".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 60,
                    payload: String::new(),
                },
            )
            .unwrap();
        assert!(!scheduler.cancel_blocking("other", "one").unwrap());
        assert!(scheduler.cancel_blocking("reminders", "one").unwrap());
        assert!(!scheduler.cancel_blocking("reminders", "one").unwrap());
    }

    #[test]
    fn restores_and_fires_an_overdue_persisted_job_once() {
        let db = DbHandle::open(":memory:").unwrap();
        db.scheduled_job_set_blocking(ScheduledJob {
            module: "reminders".into(),
            id: "restored".into(),
            server: "net".into(),
            channel: "#room".into(),
            owner_profile_id: None,
            due_at: now_secs() - 60,
            payload: "restored payload".into(),
            created_at: now_secs() - 120,
        })
        .unwrap();
        let log = LogBus::new(16);
        let (deliveries, mut rx) = mpsc::channel(2);
        let scheduler = SchedulerHandle::spawn(db, deliveries, log);
        let delivery = rx.blocking_recv().expect("restored delivery");
        assert!(matches!(
            delivery.envelope.event,
            Event::Timer { ref id, .. } if id == "restored"
        ));
        delivery.completion.finish(true);
        for _ in 0..20 {
            if scheduler
                .list_blocking("reminders", None, None)
                .unwrap()
                .is_empty()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("restored job was delivered more than once");
    }

    #[test]
    fn rejected_delivery_remains_pending_for_module_reinstall() {
        let db = DbHandle::open(":memory:").unwrap();
        db.scheduled_job_set_blocking(ScheduledJob {
            module: "missing".into(),
            id: "pending".into(),
            server: "net".into(),
            channel: "#room".into(),
            owner_profile_id: None,
            due_at: now_secs() - 1,
            payload: String::new(),
            created_at: now_secs() - 2,
        })
        .unwrap();
        let log = LogBus::new(16);
        let (deliveries, mut rx) = mpsc::channel(2);
        let scheduler = SchedulerHandle::spawn(db, deliveries, log);
        let delivery = rx.blocking_recv().expect("overdue delivery attempt");
        delivery.completion.finish(false);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(
            scheduler
                .list_blocking("missing", None, None)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn scheduler_accepts_requests_while_a_delivery_is_in_flight() {
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(16);
        let (deliveries, mut rx) = mpsc::channel(2);
        let scheduler = SchedulerHandle::spawn(db, deliveries, log);
        scheduler
            .set_blocking(
                "hunt",
                ScheduleSet {
                    id: "due".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 1,
                    payload: String::new(),
                },
            )
            .unwrap();

        let delivery = rx.blocking_recv().expect("scheduled delivery");
        scheduler
            .set_blocking(
                "hunt",
                ScheduleSet {
                    id: "next".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 60,
                    payload: String::new(),
                },
            )
            .expect("timer handlers may schedule their next job");
        delivery.completion.finish(true);

        let jobs = scheduler.list_blocking("hunt", None, None).unwrap();
        assert!(jobs.iter().any(|job| job.id == "next"));
    }

    #[test]
    fn old_completion_does_not_delete_a_replacement_job() {
        let db = DbHandle::open(":memory:").unwrap();
        let log = LogBus::new(16);
        let (deliveries, mut rx) = mpsc::channel(2);
        let scheduler = SchedulerHandle::spawn(db, deliveries, log);
        scheduler
            .set_blocking(
                "hunt",
                ScheduleSet {
                    id: "same-id".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 1,
                    payload: "old".into(),
                },
            )
            .unwrap();

        let old_delivery = rx.blocking_recv().expect("scheduled delivery");
        scheduler
            .set_blocking(
                "hunt",
                ScheduleSet {
                    id: "same-id".into(),
                    server: "net".into(),
                    channel: "#room".into(),
                    owner_profile_id: None,
                    due_at: now_secs() + 60,
                    payload: "replacement".into(),
                },
            )
            .unwrap();
        old_delivery.completion.finish(true);

        let jobs = scheduler.list_blocking("hunt", None, None).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].payload, "replacement");
    }
}
