//! Admission grants per-class caps; placement leases a whole request on one host.
//! This prototype seam has no transport or persistence. The production engine
//! will own the local observations supplied to admission and placement.
//! Prototype input and invariant failures panic; production integration must
//! surface explicit errors at those boundaries.
use std::collections::BTreeMap;

pub type Counts = BTreeMap<u64, u64>;
#[derive(Clone, Debug)]
pub struct Request {
    pub convoy: u64,
    pub class: u64,
    pub shape: Counts,
    pub priority: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Waiting,
    Requested(u64),
    Realised(u64),
    Gone,
}
#[derive(Clone, Debug)]
pub struct Vessel {
    pub request: Request,
    pub phase: Phase,
}
#[derive(Default)]
pub struct Placement {
    quotas: BTreeMap<u64, Counts>,
    vessels: BTreeMap<u64, Vessel>,
    capacities: BTreeMap<u64, Counts>,
}
impl Placement {
    pub fn new(capacities: BTreeMap<u64, Counts>) -> Self {
        Self { capacities, ..Self::default() }
    }
    pub fn vessels(&self) -> &BTreeMap<u64, Vessel> {
        &self.vessels
    }

    pub fn admit(&mut self, convoy: u64, quota: Counts, promises: usize) -> bool {
        if promises == 0 || self.quotas.contains_key(&convoy) {
            return false;
        }
        self.quotas.insert(convoy, quota);
        true
    }
    /// Reports acceptance only: unknown convoys and invalid changes both return
    /// false. This prototype exposes no distinction between refusal reasons.
    pub fn readmit(&mut self, convoy: u64, quota: Counts) -> bool {
        let Some(old) = self.quotas.get(&convoy) else {
            return false;
        };
        // Readmission raises counts within the original abstract class domain.
        if old.len() != quota.len() || old.iter().any(|(k, v)| quota.get(k).is_none_or(|new| new < v)) {
            return false;
        }
        self.quotas.insert(convoy, quota);
        true
    }
    pub fn request(&mut self, id: u64, request: Request) -> bool {
        if self.vessels.contains_key(&id) {
            return false;
        }
        self.vessels.insert(id, Vessel { request, phase: Phase::Waiting });
        true
    }
    fn host(phase: &Phase) -> Option<u64> {
        match phase {
            Phase::Requested(host) | Phase::Realised(host) => Some(*host),
            _ => None,
        }
    }
    pub fn available(&self, host: u64) -> Counts {
        let mut free = self.capacities.get(&host).expect("known host").clone();
        for vessel in self.vessels.values().filter(|v| Self::host(&v.phase) == Some(host)) {
            for (dim, amount) in &vessel.request.shape {
                let remaining = free.get_mut(dim).expect("known dimension");
                *remaining = remaining.checked_sub(*amount).expect("host leases stay within capacity");
            }
        }
        free
    }
    fn fits(request: &Request, free: &Counts) -> bool {
        request.shape.iter().all(|(d, n)| free.get(d).is_some_and(|f| n <= f))
    }
    pub fn place(&mut self, id: u64, host: u64) -> bool {
        let Some(vessel) = self.vessels.get(&id) else {
            return false;
        };
        if vessel.phase != Phase::Waiting || !self.capacities.contains_key(&host) {
            return false;
        }
        let r = &vessel.request;
        let cap = self.quotas.get(&r.convoy).and_then(|q| q.get(&r.class)).copied().unwrap_or(0);
        let live = self
            .vessels
            .values()
            .filter(|v| v.request.convoy == r.convoy && v.request.class == r.class && Self::host(&v.phase).is_some())
            .count() as u64;
        let free = self.available(host);
        if live >= cap
            || !Self::fits(r, &free)
            || self.vessels.values().any(|v| v.phase == Phase::Waiting && v.request.priority > r.priority && Self::fits(&v.request, &free))
        {
            return false;
        }
        self.vessels.get_mut(&id).expect("vessel").phase = Phase::Requested(host);
        true
    }
    pub fn realise(&mut self, id: u64) -> bool {
        let Some(v) = self.vessels.get_mut(&id) else {
            return false;
        };
        let Phase::Requested(host) = v.phase else {
            return false;
        };
        v.phase = Phase::Realised(host);
        true
    }
    pub fn release(&mut self, id: u64) -> bool {
        let Some(v) = self.vessels.get_mut(&id) else {
            return false;
        };
        if Self::host(&v.phase).is_none() {
            return false;
        }
        v.phase = Phase::Gone;
        true
    }
    pub fn vanish(&mut self, id: u64) -> bool {
        let Some(v) = self.vessels.get(&id) else {
            return false;
        };
        if !matches!(v.phase, Phase::Realised(_)) {
            return false;
        }
        self.release(id)
    }
}
