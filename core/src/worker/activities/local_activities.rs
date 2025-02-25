use crate::{
    abstractions::{MeteredSemaphore, OwnedMeteredSemPermit},
    protosext::ValidScheduleLA,
    retry_logic::RetryPolicyExt,
    MetricsContext, TaskToken,
};
use parking_lot::Mutex;
use std::{
    collections::HashMap,
    fmt::{Debug, Formatter},
    time::{Duration, Instant, SystemTime},
};
use temporal_sdk_core_protos::{
    coresdk::{
        activity_result::{Cancellation, Failure as ActFail, Success},
        activity_task::{activity_task, ActivityCancelReason, ActivityTask, Cancel, Start},
    },
    temporal::api::{common::v1::WorkflowExecution, enums::v1::TimeoutType},
};
use tokio::{
    sync::{
        mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
        Notify,
    },
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;

#[allow(clippy::large_enum_variant)] // Timeouts are relatively rare
#[derive(Debug)]
pub(crate) enum DispatchOrTimeoutLA {
    /// Send the activity task to lang
    Dispatch(ActivityTask),
    /// Notify the machines (and maybe lang) that this LA has timed out
    Timeout {
        run_id: String,
        resolution: LocalActivityResolution,
        task: Option<ActivityTask>,
    },
}

#[derive(Debug)]
pub(crate) struct LocalInFlightActInfo {
    pub la_info: NewLocalAct,
    pub dispatch_time: Instant,
    pub attempt: u32,
    _permit: OwnedMeteredSemPermit,
}

#[derive(Debug, Clone)]
pub(crate) enum LocalActivityExecutionResult {
    Completed(Success),
    Failed(ActFail),
    TimedOut(ActFail),
    Cancelled(Cancellation),
}
impl LocalActivityExecutionResult {
    pub(crate) fn empty_cancel() -> Self {
        Self::Cancelled(Cancellation::from_details(None))
    }
    pub(crate) fn timeout(tt: TimeoutType) -> Self {
        Self::TimedOut(ActFail::timeout(tt))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct LocalActivityResolution {
    pub seq: u32,
    pub result: LocalActivityExecutionResult,
    pub runtime: Duration,
    pub attempt: u32,
    pub backoff: Option<prost_types::Duration>,
    pub original_schedule_time: Option<SystemTime>,
}

#[derive(Clone)]
pub(crate) struct NewLocalAct {
    pub schedule_cmd: ValidScheduleLA,
    pub workflow_type: String,
    pub workflow_exec_info: WorkflowExecution,
    pub schedule_time: SystemTime,
}
impl Debug for NewLocalAct {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LocalActivity({}, {})",
            self.schedule_cmd.seq, self.schedule_cmd.activity_type
        )
    }
}

#[derive(Debug, derive_more::From)]
#[allow(clippy::large_enum_variant)]
pub(crate) enum LocalActRequest {
    New(NewLocalAct),
    Cancel(ExecutingLAId),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub(crate) struct ExecutingLAId {
    pub run_id: String,
    pub seq_num: u32,
}

pub(crate) struct LocalActivityManager {
    /// Just so we can provide activity tasks the same namespace as the worker
    namespace: String,
    /// Constrains number of currently executing local activities
    semaphore: MeteredSemaphore,
    /// Sink for new activity execution requests
    act_req_tx: UnboundedSender<NewOrRetry>,
    /// Cancels need a different queue since they should be taken first, and don't take a permit
    cancels_req_tx: UnboundedSender<CancelOrTimeout>,
    /// Wakes every time a complete is processed
    complete_notify: Notify,

    rcvs: tokio::sync::Mutex<RcvChans>,
    shutdown_complete_tok: CancellationToken,
    dat: Mutex<LAMData>,
}

struct LAMData {
    /// Activities that have been issued to lang but not yet completed
    outstanding_activity_tasks: HashMap<TaskToken, LocalInFlightActInfo>,
    id_to_tt: HashMap<ExecutingLAId, TaskToken>,
    /// Tasks for activities which are currently backing off. May be used to cancel retrying them.
    backing_off_tasks: HashMap<ExecutingLAId, JoinHandle<()>>,
    /// Tasks for timing out activities which are currently in the queue or dispatched.
    timeout_tasks: HashMap<ExecutingLAId, TimeoutBag>,
    next_tt_num: u32,
}

impl LAMData {
    fn gen_next_token(&mut self) -> TaskToken {
        self.next_tt_num += 1;
        TaskToken::new_local_activity_token(self.next_tt_num.to_le_bytes())
    }
}

impl LocalActivityManager {
    pub(crate) fn new(
        max_concurrent: usize,
        namespace: String,
        metrics_context: MetricsContext,
    ) -> Self {
        let (act_req_tx, act_req_rx) = unbounded_channel();
        let (cancels_req_tx, cancels_req_rx) = unbounded_channel();
        let shutdown_complete_tok = CancellationToken::new();
        Self {
            namespace,
            semaphore: MeteredSemaphore::new(
                max_concurrent,
                metrics_context,
                MetricsContext::available_task_slots,
            ),
            act_req_tx,
            cancels_req_tx,
            complete_notify: Notify::new(),
            rcvs: tokio::sync::Mutex::new(RcvChans {
                act_req_rx,
                cancels_req_rx,
                shutdown: shutdown_complete_tok.clone(),
            }),
            shutdown_complete_tok,
            dat: Mutex::new(LAMData {
                outstanding_activity_tasks: Default::default(),
                id_to_tt: Default::default(),
                backing_off_tasks: Default::default(),
                timeout_tasks: Default::default(),
                next_tt_num: 0,
            }),
        }
    }

    #[cfg(test)]
    fn test(max_concurrent: usize) -> Self {
        Self::new(
            max_concurrent,
            "fake_ns".to_string(),
            MetricsContext::default(),
        )
    }

    #[cfg(test)]
    pub(crate) fn num_outstanding(&self) -> usize {
        self.dat.lock().outstanding_activity_tasks.len()
    }

    #[cfg(test)]
    fn num_in_backoff(&self) -> usize {
        self.dat.lock().backing_off_tasks.len()
    }

    pub(crate) fn enqueue(
        &self,
        reqs: impl IntoIterator<Item = LocalActRequest>,
    ) -> Vec<LocalActivityResolution> {
        let mut immediate_resolutions = vec![];
        for req in reqs {
            debug!(local_activity = ?req, "Queuing local activity");
            match req {
                LocalActRequest::New(act) => {
                    let id = ExecutingLAId {
                        run_id: act.workflow_exec_info.run_id.clone(),
                        seq_num: act.schedule_cmd.seq,
                    };
                    let mut dlock = self.dat.lock();
                    if dlock.id_to_tt.contains_key(&id) {
                        // Do not queue local activities which are in fact already executing.
                        // This can happen during evictions.
                        debug!("Tried to queue already-executing local activity {:?}", &id);
                        continue;
                    }
                    // Pre-generate and insert the task token now, before we may or may not dispatch
                    // the activity, so we can enforce idempotency. Prevents two identical LAs
                    // ending up in the queue at once.
                    let tt = dlock.gen_next_token();
                    dlock.id_to_tt.insert(id.clone(), tt);

                    // Set up timeouts for the new activity
                    match TimeoutBag::new(&act, self.cancels_req_tx.clone()) {
                        Ok(tb) => {
                            dlock.timeout_tasks.insert(id, tb);

                            self.act_req_tx
                                .send(NewOrRetry::New(act))
                                .expect("Receive half of LA request channel cannot be dropped");
                        }
                        Err(res) => immediate_resolutions.push(res),
                    }
                }
                LocalActRequest::Cancel(id) => {
                    let mut dlock = self.dat.lock();

                    // First check if this ID is currently backing off, if so abort the backoff
                    // task
                    if let Some(t) = dlock.backing_off_tasks.remove(&id) {
                        t.abort();
                        immediate_resolutions.push(LocalActivityResolution {
                            seq: id.seq_num,
                            result: LocalActivityExecutionResult::Cancelled(
                                Cancellation::from_details(None),
                            ),
                            runtime: Duration::from_secs(0),
                            attempt: 0,
                            backoff: None,
                            original_schedule_time: None,
                        });
                        continue;
                    }

                    if let Some(tt) = dlock.id_to_tt.get(&id) {
                        self.cancels_req_tx
                            .send(CancelOrTimeout::Cancel(ActivityTask {
                                task_token: tt.0.clone(),
                                variant: Some(activity_task::Variant::Cancel(Cancel {
                                    reason: ActivityCancelReason::Cancelled as i32,
                                })),
                            }))
                            .expect("Receive half of LA cancel channel cannot be dropped");
                    }
                }
            }
        }
        immediate_resolutions
    }

    /// Returns the next pending local-activity related action, or None if shutdown has initiated
    /// and there are no more remaining actions to take.
    pub(crate) async fn next_pending(&self) -> Option<DispatchOrTimeoutLA> {
        let (new_or_retry, permit) = match self.rcvs.lock().await.next(&self.semaphore).await? {
            NewOrCancel::Cancel(c) => {
                return match c {
                    CancelOrTimeout::Cancel(c) => Some(DispatchOrTimeoutLA::Dispatch(c)),
                    CancelOrTimeout::Timeout {
                        run_id,
                        resolution,
                        dispatch_cancel,
                    } => {
                        let task = if dispatch_cancel {
                            let tt = self
                                .dat
                                .lock()
                                .id_to_tt
                                .get(&ExecutingLAId {
                                    run_id: run_id.clone(),
                                    seq_num: resolution.seq,
                                })
                                .map(Clone::clone);
                            if let Some(task_token) = tt {
                                self.complete(&task_token, &resolution.result);
                                Some(ActivityTask {
                                    task_token: task_token.0,
                                    variant: Some(activity_task::Variant::Cancel(Cancel {
                                        reason: ActivityCancelReason::TimedOut as i32,
                                    })),
                                })
                            } else {
                                None
                            }
                        } else {
                            None
                        };
                        Some(DispatchOrTimeoutLA::Timeout {
                            run_id,
                            resolution,
                            task,
                        })
                    }
                };
            }
            NewOrCancel::New(n, perm) => (n, perm),
        };

        // It is important that there are no await points after receiving from the channel, as
        // it would mean dropping this future would cause us to drop the activity request.
        let (new_la, attempt) = match new_or_retry {
            NewOrRetry::New(n) => {
                let explicit_attempt_num_or_1 = n.schedule_cmd.attempt.max(1);
                (n, explicit_attempt_num_or_1)
            }
            NewOrRetry::Retry { in_flight, attempt } => (in_flight, attempt),
        };
        let orig = new_la.clone();
        let id = ExecutingLAId {
            run_id: new_la.workflow_exec_info.run_id.clone(),
            seq_num: new_la.schedule_cmd.seq,
        };
        let sa = new_la.schedule_cmd;

        let mut dat = self.dat.lock();
        // If this request originated from a local backoff task, clear the entry for it. We
        // don't await the handle because we know it must already be done, and there's no
        // meaningful value.
        dat.backing_off_tasks.remove(&id);

        // If this task sat in the queue for too long, return a timeout for it instead
        if let Some(s2s) = sa.schedule_to_start_timeout.as_ref() {
            let sat_for = new_la.schedule_time.elapsed().unwrap_or_default();
            if sat_for > *s2s {
                return Some(DispatchOrTimeoutLA::Timeout {
                    run_id: new_la.workflow_exec_info.run_id,
                    resolution: LocalActivityResolution {
                        seq: sa.seq,
                        result: LocalActivityExecutionResult::timeout(TimeoutType::ScheduleToStart),
                        runtime: sat_for,
                        attempt,
                        backoff: None,
                        original_schedule_time: Some(new_la.schedule_time),
                    },
                    task: None,
                });
            }
        }

        let tt = dat
            .id_to_tt
            .get(&id)
            .expect("Task token must exist")
            .clone();
        dat.outstanding_activity_tasks.insert(
            tt.clone(),
            LocalInFlightActInfo {
                la_info: orig,
                dispatch_time: Instant::now(),
                attempt,
                _permit: permit,
            },
        );
        if let Some(to) = dat.timeout_tasks.get_mut(&id) {
            to.mark_started();
        }

        let (schedule_to_close, start_to_close) = sa.close_timeouts.into_sched_and_start();
        Some(DispatchOrTimeoutLA::Dispatch(ActivityTask {
            task_token: tt.0,
            variant: Some(activity_task::Variant::Start(Start {
                workflow_namespace: self.namespace.clone(),
                workflow_type: new_la.workflow_type,
                workflow_execution: Some(new_la.workflow_exec_info),
                activity_id: sa.activity_id,
                activity_type: sa.activity_type,
                header_fields: sa.headers,
                input: sa.arguments,
                heartbeat_details: vec![],
                scheduled_time: Some(new_la.schedule_time.into()),
                current_attempt_scheduled_time: Some(new_la.schedule_time.into()),
                started_time: Some(SystemTime::now().into()),
                attempt,
                schedule_to_close_timeout: schedule_to_close.and_then(|d| d.try_into().ok()),
                start_to_close_timeout: start_to_close.and_then(|d| d.try_into().ok()),
                heartbeat_timeout: None,
                retry_policy: Some(sa.retry_policy),
                is_local: true,
            })),
        }))
    }

    /// Mark a local activity as having completed (pass, fail, or cancelled)
    pub(crate) fn complete(
        &self,
        task_token: &TaskToken,
        status: &LocalActivityExecutionResult,
    ) -> LACompleteAction {
        let mut dlock = self.dat.lock();
        if let Some(info) = dlock.outstanding_activity_tasks.remove(task_token) {
            let exec_id = ExecutingLAId {
                run_id: info.la_info.workflow_exec_info.run_id.clone(),
                seq_num: info.la_info.schedule_cmd.seq,
            };
            dlock.id_to_tt.remove(&exec_id);

            match status {
                LocalActivityExecutionResult::Completed(_)
                | LocalActivityExecutionResult::TimedOut(_)
                | LocalActivityExecutionResult::Cancelled { .. } => {
                    // Timeouts are included in this branch since they are not retried
                    self.complete_notify.notify_one();
                    LACompleteAction::Report(info)
                }
                LocalActivityExecutionResult::Failed(f) => {
                    if let Some(backoff_dur) = info.la_info.schedule_cmd.retry_policy.should_retry(
                        info.attempt as usize,
                        f.failure
                            .as_ref()
                            .and_then(|f| f.maybe_application_failure()),
                    ) {
                        let will_use_timer =
                            backoff_dur > info.la_info.schedule_cmd.local_retry_threshold;
                        debug!(run_id = %info.la_info.workflow_exec_info.run_id,
                               seq_num = %info.la_info.schedule_cmd.seq,
                               attempt = %info.attempt,
                               will_use_timer,
                            "Local activity failed, will retry after backing off for {:?}",
                             backoff_dur
                        );
                        if will_use_timer {
                            // We want this to be reported, as the workflow will mark this
                            // failure down, then start a timer for backoff.
                            return LACompleteAction::LangDoesTimerBackoff(
                                backoff_dur.try_into().expect("backoff fits into proto"),
                                info,
                            );
                        }
                        // Immediately create a new task token for the to-be-retried LA
                        let tt = dlock.gen_next_token();
                        dlock.id_to_tt.insert(exec_id.clone(), tt);

                        // Send the retry request after waiting the backoff duration
                        let send_chan = self.act_req_tx.clone();
                        let jh = tokio::spawn(async move {
                            tokio::time::sleep(backoff_dur).await;

                            send_chan
                                .send(NewOrRetry::Retry {
                                    in_flight: info.la_info,
                                    attempt: info.attempt + 1,
                                })
                                .expect("Receive half of LA request channel cannot be dropped");
                        });
                        dlock.backing_off_tasks.insert(exec_id, jh);

                        LACompleteAction::WillBeRetried
                    } else {
                        LACompleteAction::Report(info)
                    }
                }
            }
        } else {
            LACompleteAction::Untracked
        }
    }

    pub(crate) async fn shutdown_and_wait_all_finished(&self) {
        while !self.dat.lock().outstanding_activity_tasks.is_empty() {
            self.complete_notify.notified().await;
        }
        self.shutdown_complete_tok.cancel();
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Most will be reported
pub(crate) enum LACompleteAction {
    /// Caller should report the status to the workflow
    Report(LocalInFlightActInfo),
    /// Lang needs to be told to do the schedule-a-timer-then-rerun hack
    LangDoesTimerBackoff(prost_types::Duration, LocalInFlightActInfo),
    /// The activity will be re-enqueued for another attempt (and so status should not be reported
    /// to the workflow)
    WillBeRetried,
    /// The activity was unknown
    Untracked,
}

#[derive(Debug)]
enum NewOrRetry {
    New(NewLocalAct),
    Retry {
        in_flight: NewLocalAct,
        attempt: u32,
    },
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
enum CancelOrTimeout {
    Cancel(ActivityTask),
    Timeout {
        run_id: String,
        resolution: LocalActivityResolution,
        dispatch_cancel: bool,
    },
}

enum NewOrCancel {
    New(NewOrRetry, OwnedMeteredSemPermit),
    Cancel(CancelOrTimeout),
}

struct RcvChans {
    /// Activities that need to be executed by lang
    act_req_rx: UnboundedReceiver<NewOrRetry>,
    /// Cancels to send to lang or apply internally
    cancels_req_rx: UnboundedReceiver<CancelOrTimeout>,
    shutdown: CancellationToken,
}

impl RcvChans {
    async fn next(&mut self, new_sem: &MeteredSemaphore) -> Option<NewOrCancel> {
        tokio::select! {
            cancel = async { self.cancels_req_rx.recv().await } => {
                Some(NewOrCancel::Cancel(cancel.expect("Send halves of LA manager are not dropped")))
            }
            (maybe_new_or_retry, perm) = async {
                // Wait for a permit to take a task and forget it. Permits are removed until a
                // completion.
                let perm = new_sem.acquire_owned().await.expect("is never closed");
                (self.act_req_rx.recv().await, perm)
            } => Some(NewOrCancel::New(
                maybe_new_or_retry.expect("Send halves of LA manager are not dropped"), perm
            )),
            _ = self.shutdown.cancelled() => None
        }
    }
}

struct TimeoutBag {
    sched_to_close_handle: JoinHandle<()>,
    start_to_close_dur_and_dat: Option<(Duration, CancelOrTimeout)>,
    start_to_close_handle: Option<JoinHandle<()>>,
    cancel_chan: UnboundedSender<CancelOrTimeout>,
}

impl TimeoutBag {
    /// Create new timeout tasks for the provided local activity. This must be called as soon
    /// as request to schedule it arrives.
    ///
    /// Returns error in the event the activity is *already* timed out
    fn new(
        new_la: &NewLocalAct,
        cancel_chan: UnboundedSender<CancelOrTimeout>,
    ) -> Result<TimeoutBag, LocalActivityResolution> {
        let (schedule_to_close, start_to_close) =
            new_la.schedule_cmd.close_timeouts.into_sched_and_start();

        let resolution = LocalActivityResolution {
            seq: new_la.schedule_cmd.seq,
            result: LocalActivityExecutionResult::timeout(TimeoutType::ScheduleToClose),
            runtime: Default::default(),
            attempt: new_la.schedule_cmd.attempt,
            backoff: None,
            original_schedule_time: Some(new_la.schedule_time),
        };
        // Remove any time already elapsed since the scheduling time
        let schedule_to_close = schedule_to_close
            .map(|s2c| s2c.saturating_sub(new_la.schedule_time.elapsed().unwrap_or_default()));
        if let Some(ref s2c) = schedule_to_close {
            if s2c.is_zero() {
                return Err(resolution);
            }
        }
        let timeout_dat = CancelOrTimeout::Timeout {
            run_id: new_la.workflow_exec_info.run_id.clone(),
            resolution,
            dispatch_cancel: true,
        };
        let start_to_close_dur_and_dat = start_to_close.map(|d| (d, timeout_dat.clone()));
        let fut_dat = schedule_to_close.map(|s2c| (s2c, timeout_dat));

        let cancel_chan_clone = cancel_chan.clone();
        let scheduling = tokio::spawn(async move {
            if let Some((timeout, dat)) = fut_dat {
                sleep(timeout).await;
                cancel_chan_clone
                    .send(dat)
                    .expect("receive half not dropped");
            }
        });
        Ok(TimeoutBag {
            sched_to_close_handle: scheduling,
            start_to_close_dur_and_dat,
            start_to_close_handle: None,
            cancel_chan,
        })
    }

    /// Must be called once the associated local activity has been started / dispatched to lang.
    fn mark_started(&mut self) {
        if let Some((start_to_close, mut dat)) = self.start_to_close_dur_and_dat.take() {
            let started_t = Instant::now();
            let cchan = self.cancel_chan.clone();
            self.start_to_close_handle = Some(tokio::spawn(async move {
                sleep(start_to_close).await;
                if let CancelOrTimeout::Timeout { resolution, .. } = &mut dat {
                    resolution.result =
                        LocalActivityExecutionResult::timeout(TimeoutType::StartToClose);
                    resolution.runtime = started_t.elapsed();
                }

                cchan.send(dat).expect("receive half not dropped");
            }));
        }
    }
}

impl Drop for TimeoutBag {
    fn drop(&mut self) {
        self.sched_to_close_handle.abort();
        if let Some(x) = self.start_to_close_handle.as_ref() {
            x.abort()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{prost_dur, protosext::LACloseTimeouts};
    use temporal_sdk_core_protos::temporal::api::{
        common::v1::RetryPolicy,
        failure::v1::{failure::FailureInfo, ApplicationFailureInfo, Failure},
    };
    use tokio::{sync::mpsc::error::TryRecvError, task::yield_now};

    impl DispatchOrTimeoutLA {
        fn unwrap(self) -> ActivityTask {
            match self {
                DispatchOrTimeoutLA::Dispatch(t) => t,
                DispatchOrTimeoutLA::Timeout { .. } => {
                    panic!("Timeout returned when expected a task")
                }
            }
        }
    }

    #[tokio::test]
    async fn max_concurrent_respected() {
        let lam = LocalActivityManager::test(1);
        lam.enqueue((1..=50).map(|i| {
            NewLocalAct {
                schedule_cmd: ValidScheduleLA {
                    seq: i,
                    activity_id: i.to_string(),
                    ..Default::default()
                },
                workflow_type: "".to_string(),
                workflow_exec_info: Default::default(),
                schedule_time: SystemTime::now(),
            }
            .into()
        }));
        for i in 1..=50 {
            let next = lam.next_pending().await.unwrap().unwrap();
            assert_matches!(
                next.variant.unwrap(),
                activity_task::Variant::Start(Start {activity_id, ..})
                    if activity_id == i.to_string()
            );
            let next_tt = TaskToken(next.task_token);
            let complete_branch = async {
                lam.complete(
                    &next_tt,
                    &LocalActivityExecutionResult::Completed(Default::default()),
                )
            };
            tokio::select! {
                // Next call will not resolve until we complete the first
                _ = lam.next_pending() => {
                    panic!("Branch must not be selected")
                }
                _ = complete_branch => {}
            }
        }
    }

    #[tokio::test]
    async fn no_work_doesnt_deadlock_with_complete() {
        let lam = LocalActivityManager::test(5);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: Default::default(),
            schedule_time: SystemTime::now(),
        }
        .into()]);

        let next = lam.next_pending().await.unwrap().unwrap();
        let tt = TaskToken(next.task_token);
        tokio::select! {
            biased;

            _ = lam.next_pending() => {
                panic!("Complete branch must win")
            }
            _ = async {
                // Spin until the receive lock has been grabbed by the call to pending, to ensure
                // it's advanced enough
                while lam.rcvs.try_lock().is_ok() { yield_now().await; }
                lam.complete(&tt, &LocalActivityExecutionResult::Completed(Default::default()));
            } => (),
        };
    }

    #[tokio::test]
    async fn can_cancel_in_flight() {
        let lam = LocalActivityManager::test(5);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        }
        .into()]);
        lam.next_pending().await.unwrap().unwrap();

        lam.enqueue([LocalActRequest::Cancel(ExecutingLAId {
            run_id: "run_id".to_string(),
            seq_num: 1,
        })]);
        let next = lam.next_pending().await.unwrap().unwrap();
        assert_matches!(next.variant.unwrap(), activity_task::Variant::Cancel(_));
    }

    #[tokio::test]
    async fn respects_timer_backoff_threshold() {
        let lam = LocalActivityManager::test(1);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                attempt: 5,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_secs(1))),
                    backoff_coefficient: 10.0,
                    maximum_interval: Some(prost_dur!(from_secs(10))),
                    maximum_attempts: 10,
                    non_retryable_error_types: vec![],
                },
                local_retry_threshold: Duration::from_secs(5),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: Default::default(),
            schedule_time: SystemTime::now(),
        }
        .into()]);

        let next = lam.next_pending().await.unwrap().unwrap();
        let tt = TaskToken(next.task_token);
        let res = lam.complete(
            &tt,
            &LocalActivityExecutionResult::Failed(Default::default()),
        );
        assert_matches!(res, LACompleteAction::LangDoesTimerBackoff(dur, info)
            if dur.seconds == 10 && info.attempt == 5
        )
    }

    #[tokio::test]
    async fn respects_non_retryable_error_types() {
        let lam = LocalActivityManager::test(1);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: "1".to_string(),
                attempt: 1,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_secs(1))),
                    backoff_coefficient: 10.0,
                    maximum_interval: Some(prost_dur!(from_secs(10))),
                    maximum_attempts: 10,
                    non_retryable_error_types: vec!["TestError".to_string()],
                },
                local_retry_threshold: Duration::from_secs(5),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: Default::default(),
            schedule_time: SystemTime::now(),
        }
        .into()]);

        let next = lam.next_pending().await.unwrap().unwrap();
        let tt = TaskToken(next.task_token);
        let res = lam.complete(
            &tt,
            &LocalActivityExecutionResult::Failed(ActFail {
                failure: Some(Failure {
                    failure_info: Some(FailureInfo::ApplicationFailureInfo(
                        ApplicationFailureInfo {
                            r#type: "TestError".to_string(),
                            non_retryable: false,
                            ..Default::default()
                        },
                    )),
                    ..Default::default()
                }),
            }),
        );
        assert_matches!(res, LACompleteAction::Report(_));
    }

    #[tokio::test]
    async fn can_cancel_during_local_backoff() {
        let lam = LocalActivityManager::test(1);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                attempt: 5,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_secs(10))),
                    backoff_coefficient: 1.0,
                    maximum_interval: Some(prost_dur!(from_secs(10))),
                    maximum_attempts: 10,
                    non_retryable_error_types: vec![],
                },
                local_retry_threshold: Duration::from_secs(500),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        }
        .into()]);

        let next = lam.next_pending().await.unwrap().unwrap();
        let tt = TaskToken(next.task_token);
        lam.complete(
            &tt,
            &LocalActivityExecutionResult::Failed(Default::default()),
        );
        // Cancel the activity, which is performing local backoff
        let immediate_res = lam.enqueue([LocalActRequest::Cancel(ExecutingLAId {
            run_id: "run_id".to_string(),
            seq_num: 1,
        })]);
        // It should not be present in the backoff tasks
        assert_eq!(lam.num_in_backoff(), 0);
        assert_eq!(lam.num_outstanding(), 0);
        // It should return a resolution to cancel
        assert_eq!(immediate_res.len(), 1);
        assert_matches!(
            immediate_res[0].result,
            LocalActivityExecutionResult::Cancelled { .. }
        );
    }

    #[tokio::test]
    async fn local_backoff_clears_handle_map_when_started() {
        let lam = LocalActivityManager::test(1);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                attempt: 5,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_millis(10))),
                    backoff_coefficient: 1.0,
                    ..Default::default()
                },
                local_retry_threshold: Duration::from_secs(500),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        }
        .into()]);

        let next = lam.next_pending().await.unwrap().unwrap();
        let tt = TaskToken(next.task_token);
        lam.complete(
            &tt,
            &LocalActivityExecutionResult::Failed(Default::default()),
        );
        lam.next_pending().await.unwrap().unwrap();
        assert_eq!(lam.num_in_backoff(), 0);
        assert_eq!(lam.num_outstanding(), 1);
    }

    #[tokio::test]
    async fn sched_to_start_timeout() {
        let lam = LocalActivityManager::test(1);
        let timeout = Duration::from_millis(100);
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                attempt: 5,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_millis(10))),
                    backoff_coefficient: 1.0,
                    ..Default::default()
                },
                local_retry_threshold: Duration::from_secs(500),
                schedule_to_start_timeout: Some(timeout),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        }
        .into()]);

        // Wait more than the timeout before grabbing the task
        sleep(timeout + Duration::from_millis(10)).await;

        assert_matches!(
            lam.next_pending().await.unwrap(),
            DispatchOrTimeoutLA::Timeout { .. }
        );
        assert_eq!(lam.num_in_backoff(), 0);
        assert_eq!(lam.num_outstanding(), 0);
    }

    #[rstest::rstest]
    #[case::schedule(true)]
    #[case::start(false)]
    #[tokio::test]
    async fn local_x_to_close_timeout(#[case] is_schedule: bool) {
        let lam = LocalActivityManager::test(1);
        let timeout = Duration::from_millis(100);
        let close_timeouts = if is_schedule {
            LACloseTimeouts::ScheduleOnly(timeout)
        } else {
            LACloseTimeouts::StartOnly(timeout)
        };
        lam.enqueue([NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                attempt: 5,
                retry_policy: RetryPolicy {
                    initial_interval: Some(prost_dur!(from_millis(10))),
                    backoff_coefficient: 1.0,
                    ..Default::default()
                },
                local_retry_threshold: Duration::from_secs(500),
                close_timeouts,
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        }
        .into()]);

        lam.next_pending().await.unwrap().unwrap();
        assert_eq!(lam.num_in_backoff(), 0);
        assert_eq!(lam.num_outstanding(), 1);

        sleep(timeout + Duration::from_millis(10)).await;
        assert_matches!(
            lam.next_pending().await.unwrap(),
            DispatchOrTimeoutLA::Timeout { .. }
        );
        assert_eq!(lam.num_outstanding(), 0);
    }

    #[tokio::test]
    async fn idempotency_enforced() {
        let lam = LocalActivityManager::test(10);
        let new_la = NewLocalAct {
            schedule_cmd: ValidScheduleLA {
                seq: 1,
                activity_id: 1.to_string(),
                ..Default::default()
            },
            workflow_type: "".to_string(),
            workflow_exec_info: WorkflowExecution {
                workflow_id: "".to_string(),
                run_id: "run_id".to_string(),
            },
            schedule_time: SystemTime::now(),
        };
        // Verify only one will get queued
        lam.enqueue([new_la.clone().into(), new_la.clone().into()]);
        lam.next_pending().await.unwrap().unwrap();
        assert_eq!(lam.num_outstanding(), 1);
        // There should be nothing else in the queue
        assert_eq!(
            lam.rcvs.lock().await.act_req_rx.try_recv().unwrap_err(),
            TryRecvError::Empty
        );

        // Verify that if we now enqueue the same act again, after the task is outstanding, we still
        // don't add it.
        lam.enqueue([new_la.into()]);
        assert_eq!(lam.num_outstanding(), 1);
        assert_eq!(
            lam.rcvs.lock().await.act_req_rx.try_recv().unwrap_err(),
            TryRecvError::Empty
        );
    }
}
