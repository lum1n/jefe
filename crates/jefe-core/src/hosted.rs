//! [`Hosted`]: a [`Jefe`] whose state lives on the job's host, with a lease
//! so one client drives at a time. It wraps `step` and adds its own
//! [`Effect::Run`]s (saves, lease renewals) to the stream, so a frontend
//! that runs effects for `Jefe` runs them for `Hosted` without more code.

use std::collections::BTreeMap;

use acc_core::session::SessionSpec;

use crate::event::{Event, Stamped};
use crate::ids::ExecId;
use crate::io::{Effect, Input, Purpose};
use crate::jefe::{Jefe, RestoreError};
use crate::job::JobSpec;
use crate::store::{self, LEASE_RENEW_MS, LEASE_TTL_MS, SAVE_DEBOUNCE_MS};

/// Exec ids above this are the wrapper's, not the loop's.
const OWN_IDS: u64 = 1 << 40;
/// Saves that may fail in a row before the wrapper stops trying.
const SAVE_FAILURES: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Own {
    Save,
    Final,
    Lease,
    Release,
}

/// A job whose state is kept on its host.
#[derive(Debug)]
pub struct Hosted {
    jefe: Jefe,
    /// This client's name in the lease.
    client: String,
    /// Saves and the lease are on. Off, `step` is `Jefe::step`.
    hosted: bool,
    next_exec: u64,
    runs: BTreeMap<ExecId, Own>,
    pending: Vec<Stamped>,
    dirty: bool,
    save_due: Option<u64>,
    saving: bool,
    save_failures: u32,
    lease_due: Option<u64>,
    leasing: bool,
    lease_lost: bool,
    released: bool,
    /// The release came back from the host (the lease file is gone).
    release_done: bool,
    finished: bool,
    now: u64,
}

impl Hosted {
    /// Starts a job and keeps its state on the host.
    pub fn start(spec: JobSpec, client: &str, now_ms: u64) -> (Self, Vec<Effect>) {
        let (jefe, effects) = Jefe::new(spec, now_ms);
        let mut h = Self::wrap(jefe, client, true, now_ms);
        h.lease_due = Some(now_ms);
        let out = h.after(effects);
        (h, out)
    }

    /// Picks a job up from the snapshot [`store::load_exec`] brought back,
    /// once [`store::lease_exec`] said this client holds the lease.
    pub fn open(
        snapshot: &str,
        specs: &[SessionSpec],
        client: &str,
        now_ms: u64,
    ) -> Result<(Self, Vec<Effect>), RestoreError> {
        let (jefe, effects) = Jefe::restore(snapshot, specs, now_ms)?;
        let finished = jefe.is_finished();
        let mut h = Self::wrap(jefe, client, true, now_ms);
        h.lease_due = Some(now_ms.saturating_add(LEASE_RENEW_MS));
        // A job that was over when it was opened has nothing to drive: its
        // state is saved once more (a look, not a change) and the lease let go.
        h.finished = finished;
        let out = h.after(effects);
        Ok((h, out))
    }

    /// A plain loop: nothing is saved on the host and no lease is taken.
    pub fn local(jefe: Jefe, now_ms: u64) -> Self {
        Self::wrap(jefe, "local", false, now_ms)
    }

    fn wrap(jefe: Jefe, client: &str, hosted: bool, now_ms: u64) -> Self {
        Self {
            jefe,
            client: client.to_owned(),
            hosted,
            next_exec: 0,
            runs: BTreeMap::new(),
            pending: Vec::new(),
            dirty: hosted,
            save_due: hosted.then_some(now_ms),
            saving: false,
            save_failures: 0,
            lease_due: None,
            leasing: false,
            lease_lost: false,
            released: false,
            release_done: false,
            finished: false,
            now: now_ms,
        }
    }

    pub fn jefe(&self) -> &Jefe {
        &self.jefe
    }

    pub fn client(&self) -> &str {
        &self.client
    }

    /// Another client took the lease; this one stopped driving.
    pub fn lease_lost(&self) -> bool {
        self.lease_lost
    }

    /// The job is over and its last save and release are done: a driver
    /// that stops before the release came back could leave the lease behind.
    pub fn is_finished(&self) -> bool {
        self.jefe.is_finished() && (!self.hosted || self.release_done)
    }

    pub fn snapshot(&self) -> String {
        self.jefe.snapshot()
    }

    /// When the host must send the next [`Input::Tick`].
    pub fn next_deadline(&self) -> Option<u64> {
        let own = [
            self.save_due.filter(|_| self.dirty && !self.saving),
            self.lease_due.filter(|_| !self.leasing && !self.lease_lost),
        ];
        self.jefe
            .next_deadline()
            .into_iter()
            .chain(own.into_iter().flatten())
            .min()
    }

    /// Advances the loop; the wrapper's own exec results are taken here.
    pub fn step(&mut self, input: Input, now_ms: u64) -> Vec<Effect> {
        self.now = now_ms;
        if self.lease_lost {
            return Vec::new();
        }
        if let Input::ExecDone { id, code, stdout, .. } = &input
            && let Some(own) = self.runs.remove(id)
        {
            let mut out = self.on_own(own, *code, stdout);
            out.extend(self.settle());
            return out;
        }
        let effects = self.jefe.step(input, now_ms);
        self.after(effects)
    }

    /// Gives the lease up on purpose (the app goes to the background):
    /// workers keep going, decisions wait for the next client.
    pub fn release(&mut self, now_ms: u64) -> Vec<Effect> {
        self.now = now_ms;
        if !self.hosted || self.released {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.dirty {
            out.extend(self.save());
        }
        self.released = true;
        self.lease_due = None;
        out.push(self.own(
            store::release_exec(&self.jefe.job().id, &self.client),
            Own::Release,
            Purpose::Release,
        ));
        out
    }

    fn on_own(&mut self, own: Own, code: i32, stdout: &str) -> Vec<Effect> {
        match own {
            Own::Save => {
                self.saving = false;
                if code == 0 {
                    self.save_failures = 0;
                } else {
                    self.save_failures += 1;
                    if self.save_failures == SAVE_FAILURES {
                        let effects = self.jefe.record(
                            Event::Notice {
                                message: "saving the job on its host keeps failing; this client stops trying"
                                    .to_owned(),
                            },
                            self.now,
                        );
                        return self.collect(effects);
                    }
                }
            }
            Own::Lease => {
                self.leasing = false;
                match store::parse_lease(stdout) {
                    Some(l) if !l.ok => {
                        self.lease_lost = true;
                        self.lease_due = None;
                        let effects = self.jefe.record(Event::LeaseLost { holder: l.holder }, self.now);
                        return self.collect(effects);
                    }
                    // Held, or the host didn't answer: the next renewal tells.
                    _ => {}
                }
            }
            Own::Release => self.release_done = true,
            Own::Final => {}
        }
        Vec::new()
    }

    /// Keeps the loop's events for the next save, passing everything on.
    fn collect(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        if !self.hosted {
            return effects;
        }
        let mut out = Vec::with_capacity(effects.len());
        for e in effects {
            match &e {
                Effect::Emit(s) => {
                    self.pending.push(s.clone());
                    self.dirty = true;
                    if let Event::WorkerReported { worker, .. } = &s.event
                        && let Some(text) = self.jefe.job().worker(worker).and_then(|w| w.last_message())
                    {
                        let exec = store::worker_final_exec(&self.jefe.job().id, worker.as_str(), &text);
                        out.push(self.own(exec, Own::Final, Purpose::Save));
                    }
                }
                Effect::Finished { .. } => {
                    self.dirty = true;
                    self.finished = true;
                }
                _ => {}
            }
            out.push(e);
        }
        out
    }

    fn after(&mut self, effects: Vec<Effect>) -> Vec<Effect> {
        let mut out = self.collect(effects);
        out.extend(self.settle());
        out
    }

    /// Saves and renews when due.
    fn settle(&mut self) -> Vec<Effect> {
        if !self.hosted || self.lease_lost {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.dirty && !self.saving && self.save_failures < SAVE_FAILURES {
            let due = *self
                .save_due
                .get_or_insert(self.now.saturating_add(SAVE_DEBOUNCE_MS));
            if self.finished || due <= self.now {
                out.extend(self.save());
            } else {
                out.push(Effect::WakeIn {
                    ms: due.saturating_sub(self.now),
                });
            }
        }
        if self.finished && !self.released && !self.saving && !self.dirty {
            self.released = true;
            self.lease_due = None;
            out.push(self.own(
                store::release_exec(&self.jefe.job().id, &self.client),
                Own::Release,
                Purpose::Release,
            ));
        }
        if let Some(due) = self.lease_due
            && !self.leasing
            && !self.released
            && due <= self.now
        {
            self.leasing = true;
            self.lease_due = Some(self.now.saturating_add(LEASE_RENEW_MS));
            let exec = store::lease_exec(&self.jefe.job().id, &self.client, LEASE_TTL_MS);
            out.push(self.own(exec, Own::Lease, Purpose::Lease));
        }
        out
    }

    fn save(&mut self) -> Vec<Effect> {
        let job = self.jefe.job();
        let events = std::mem::take(&mut self.pending);
        let exec = store::save_exec(&job.id, &self.jefe.snapshot(), job.status, &job.goal, &events);
        self.dirty = false;
        self.save_due = None;
        self.saving = true;
        vec![self.own(exec, Own::Save, Purpose::Save)]
    }

    fn own(&mut self, exec: acc_core::Exec, own: Own, purpose: Purpose) -> Effect {
        self.next_exec += 1;
        let id = ExecId(OWN_IDS + self.next_exec);
        self.runs.insert(id, own);
        Effect::Run {
            id,
            host: self.jefe.job().host.clone(),
            exec,
            purpose,
        }
    }
}
