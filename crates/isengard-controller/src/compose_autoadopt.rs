//! Auto-adoption debouncer: synthesize compose for stacks the operator
//! brought up with plain `docker compose up` and write it to the
//! controller, tagged as `auto_synthesized`.
//!
//! Triggered on every heartbeat. Per-stack state machine:
//!
//! ```text
//!  fresh observation                 (unknown set)
//!     |
//!     |  heartbeat ingest hands the current container-id set
//!     v
//!  pending(set, stable_count = 1)
//!     |
//!     |  next heartbeat has the same set
//!     v
//!  pending(set, stable_count = 2)    -> SYNTHESIZE + WRITE + record adopted
//!     |
//!     |  set changed on either step -> reset stable_count to 1
//!     |  container without rich data observed -> skip entirely
//!     |  stack already has stored compose (any source) -> skip entirely
//!     v
//!  adopted
//!     |
//!     |  every subsequent heartbeat is a no-op (single short-circuit).
//!     |  drift surfaces via the doctor, not via re-adoption.
//! ```
//!
//! The tracker is in-memory only. If the controller restarts, the next
//! two heartbeats re-establish stability and the stack auto-adopts
//! again iff no operator wrote a compose in between (the
//! `compose_source == operator_written` short-circuit still applies).
//!
//! Spec: `3 Resources/Superpowers/specs/2026-05-23-isd-compose-synthesize-design.md`,
//! "Auto-adoption" section. Hard rule: **observation auto; mutation
//! never.** Even after adoption, this module never overwrites an
//! existing stored compose.
//!
//! ## What ships in this PR vs. the parallel work
//!
//! This module ships the *decision* engine: the per-stack state
//! machine, the rich-data gate, and the hand-off shape the heartbeat
//! handler uses to invoke synthesis. The actual `synthesize` function
//! and the `containers_rich` DAO land in the parallel
//! `agent-rich-container-snapshot` + `feat-controller-compose-synthesizer`
//! PRs. When those merge, the heartbeat handler in `service.rs` will
//! switch from passing `rich_container_ids: &[]` (which forces every
//! observation into [`Decision::MissingRichData`]) to passing the
//! real set queried out of `containers_rich`. The contract here does
//! not change; only the wiring does.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use isengard_storage::host::HostId;
use isengard_storage::stack::StackId;

/// Default number of consecutive heartbeats a stack must report the
/// same container-id set before the controller fires synthesis.
///
/// Two is the spec's floor: `docker compose up` brings services up
/// sequentially (compose-v2 honors `depends_on`), so a 1-heartbeat
/// trigger would write a one-service compose right as the second
/// service is creating. Two heartbeats let the agent ship two scans
/// after the last container settled.
///
/// Exposed as a constant so tests can refer to it; the production
/// tracker uses this default through [`AutoAdoptionTracker::new`].
pub const DEFAULT_HEARTBEATS_TO_STABLE: u32 = 2;

/// Key uniquely identifying a stack across the fleet. The controller's
/// `stacks` table is keyed by `(host_id, name)`; the in-memory tracker
/// uses the same pair so it can short-circuit before any DB call.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StackKey {
    /// Owning host.
    pub host_id: HostId,
    /// Stack name (compose project label, manual override, or inferred).
    pub stack_name: String,
}

/// In-memory state machine per `(host_id, stack_name)` pair.
///
/// `Pending` carries the most recently observed container-id set and
/// the consecutive-heartbeat count where that set held steady.
/// `Adopted` is sticky: once flipped, every subsequent heartbeat
/// short-circuits without touching the DB. The flip back to `Pending`
/// would require operator action (manual delete + re-discover) and is
/// intentionally not modelled here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackerState {
    /// Currently waiting for stability. `last_set` is the set we saw on
    /// the most recent heartbeat; `stable_count` is how many heartbeats
    /// in a row that exact set has been reported.
    Pending {
        /// Container ids observed on the most recent heartbeat. Sorted
        /// is not required (HashSet equality is by membership).
        last_set: HashSet<String>,
        /// How many consecutive heartbeats `last_set` has been stable.
        /// `1` after the first observation.
        stable_count: u32,
    },
    /// Auto-adoption fired. Future heartbeats no-op.
    Adopted,
}

/// Outcome of a single [`AutoAdoptionTracker::observe`] call.
///
/// Drives the caller's branch: only [`Decision::Synthesize`] requires
/// the synthesis + storage write to run. All other variants are
/// purely informational.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Stack already auto-adopted in a previous heartbeat. Caller does
    /// nothing.
    AlreadyAdopted,
    /// Stack has stored compose (operator-owned or previously
    /// auto-adopted in a prior controller run). Caller skips; the
    /// tracker forces this stack into the `Adopted` state so subsequent
    /// heartbeats short-circuit without further DB checks.
    AlreadyHasCompose,
    /// At least one container in the stack lacks rich snapshot data.
    /// Caller skips; the tracker keeps no state for this stack
    /// (the next heartbeat re-evaluates fresh).
    MissingRichData,
    /// First time the controller sees this stack with rich data.
    /// Caller skips; the tracker records the set as pending.
    NewlyTracked,
    /// Container set changed since the previous heartbeat. Caller
    /// skips; the tracker resets the stability counter to 1.
    SetChanged,
    /// Container set has been stable but the floor of stable
    /// heartbeats has not been reached yet. Caller skips; the tracker
    /// increments the stability counter.
    StableButNotYet {
        /// Stability count after this heartbeat. `1..` strictly less
        /// than [`AutoAdoptionTracker::heartbeats_to_stable`].
        stable_count: u32,
    },
    /// Stack just crossed the stability threshold. Caller MUST
    /// synthesize from `containers` and write the result with
    /// `compose_source = "auto_synthesized"`. The tracker has already
    /// flipped to `Adopted`, so a failed write does not auto-retry
    /// on the next heartbeat: the operator's `isd stack adopt
    /// --refresh` is the recovery path. (Choice point: this is
    /// "exactly once or never", not "at least once". The alternative
    /// is replaying the synthesis on the next heartbeat by deferring
    /// the `Adopted` flip until the caller signals success; in
    /// practice the failure mode is a SQL error that is just as
    /// likely on the retry, and the operator surface already has a
    /// manual fallback.)
    Synthesize,
}

/// Input to one [`AutoAdoptionTracker::observe`] call.
///
/// `container_ids` is the full operator-visible container id set for
/// this stack as reported in the heartbeat. `rich_container_ids` is
/// the subset for which the rich snapshot (`containers_rich` table)
/// has been populated; if the two sets differ, the rich-data gate
/// returns [`Decision::MissingRichData`] without advancing any
/// counter.
///
/// `has_stored_compose` reflects whether `stacks.compose_yaml IS NOT
/// NULL` for the stack. The caller checks this before invoking the
/// tracker; the tracker rechecks defensively so the
/// `AlreadyHasCompose` branch is reached even if the heartbeat handler
/// short-circuits in the wrong order.
#[derive(Debug, Clone)]
pub struct Observation<'a> {
    /// Owning host id + stack name.
    pub stack_key: StackKey,
    /// Surrogate id of the stack row, used by the caller to look up
    /// the existing compose and (on the synthesize branch) to write
    /// the new compose.
    pub stack_id: StackId,
    /// Operator-visible container ids the heartbeat reported for this
    /// stack, before rich-data filtering.
    pub container_ids: &'a [String],
    /// Subset of `container_ids` for which the rich snapshot
    /// (containers_rich) is populated. Length-mismatch with
    /// `container_ids` triggers [`Decision::MissingRichData`].
    pub rich_container_ids: &'a [String],
    /// Whether the stack already has a `compose_yaml` value in the
    /// `stacks` row. When true, the tracker short-circuits to
    /// [`Decision::AlreadyHasCompose`] regardless of state.
    pub has_stored_compose: bool,
}

/// In-memory state for the auto-adoption debouncer.
///
/// Construct once at controller boot and clone the `Arc` into every
/// heartbeat handler. The internal mutex is uncontended in the steady
/// state (one heartbeat per host every ~30s); contention only matters
/// in tests that pummel the tracker.
#[derive(Debug)]
pub struct AutoAdoptionTracker {
    /// Per-stack state. Lock is short-lived: an `observe` call only
    /// holds it for the duration of a HashSet build + compare.
    by_stack: Mutex<HashMap<StackKey, TrackerState>>,
    /// Stability floor before [`Decision::Synthesize`] fires. Set at
    /// construction; the default ([`DEFAULT_HEARTBEATS_TO_STABLE`])
    /// is `2`.
    heartbeats_to_stable: u32,
}

impl AutoAdoptionTracker {
    /// Build a tracker with [`DEFAULT_HEARTBEATS_TO_STABLE`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_HEARTBEATS_TO_STABLE)
    }

    /// Build a tracker with a custom stability floor. Tests use this
    /// with `1` to skip the debounce; production sticks to the
    /// default. A floor of `0` is rejected upward to `1` because
    /// "synthesize on first observation" would defeat the whole
    /// debouncer (and the spec explicitly forbids it).
    #[must_use]
    pub fn with_threshold(heartbeats_to_stable: u32) -> Self {
        Self {
            by_stack: Mutex::new(HashMap::new()),
            heartbeats_to_stable: heartbeats_to_stable.max(1),
        }
    }

    /// Configured stability floor. Exposed so tests can assert without
    /// hard-coding the constant.
    #[must_use]
    pub fn heartbeats_to_stable(&self) -> u32 {
        self.heartbeats_to_stable
    }

    /// Apply one heartbeat observation.
    ///
    /// See [`Decision`] for the meaning of every return variant. The
    /// caller invokes [`Decision::Synthesize`] by reading
    /// `containers_rich` rows for `obs.rich_container_ids`, building
    /// the synthesizer's input, and calling
    /// `inventory.set_stack_compose` with
    /// `ComposeSource::AutoSynthesized`.
    pub fn observe(&self, obs: &Observation<'_>) -> Decision {
        // Short-circuit 1: stack already has a stored compose. Lock
        // ahead of the rich-data check so we leave a sticky `Adopted`
        // marker; future heartbeats then bail out at the top.
        if obs.has_stored_compose {
            let mut g = self.lock_state();
            g.insert(obs.stack_key.clone(), TrackerState::Adopted);
            return Decision::AlreadyHasCompose;
        }

        // Short-circuit 2: rich-data gate. If any container in the
        // stack is missing rich data, synthesis would be incomplete.
        // Spec: "Skip auto-adopt for any stack with at least one
        // container missing rich data."
        if obs.container_ids.len() != obs.rich_container_ids.len() {
            return Decision::MissingRichData;
        }

        let current_set: HashSet<String> = obs.rich_container_ids.iter().cloned().collect();
        let mut g = self.lock_state();

        match g.get(&obs.stack_key).cloned() {
            Some(TrackerState::Adopted) => Decision::AlreadyAdopted,
            None => {
                // First observation with rich data. Plant a Pending
                // marker; the caller skips this heartbeat.
                g.insert(
                    obs.stack_key.clone(),
                    TrackerState::Pending {
                        last_set: current_set,
                        stable_count: 1,
                    },
                );
                Decision::NewlyTracked
            }
            Some(TrackerState::Pending {
                last_set,
                stable_count,
            }) => {
                if last_set != current_set {
                    g.insert(
                        obs.stack_key.clone(),
                        TrackerState::Pending {
                            last_set: current_set,
                            stable_count: 1,
                        },
                    );
                    return Decision::SetChanged;
                }
                let next = stable_count + 1;
                if next >= self.heartbeats_to_stable {
                    g.insert(obs.stack_key.clone(), TrackerState::Adopted);
                    Decision::Synthesize
                } else {
                    g.insert(
                        obs.stack_key.clone(),
                        TrackerState::Pending {
                            last_set: current_set,
                            stable_count: next,
                        },
                    );
                    Decision::StableButNotYet { stable_count: next }
                }
            }
        }
    }

    /// Test helper: number of distinct stacks the tracker is currently
    /// keeping state for. Not part of the production surface; pinning
    /// to `#[cfg(test)]` would force every test to bypass the public
    /// API.
    #[doc(hidden)]
    pub fn tracked_stacks(&self) -> usize {
        self.lock_state().len()
    }

    /// Lock the inner state. Panics if poisoned (a panic inside a
    /// holding thread would corrupt the map anyway; recovering is not
    /// meaningful for an in-memory cache).
    fn lock_state(&self) -> std::sync::MutexGuard<'_, HashMap<StackKey, TrackerState>> {
        self.by_stack
            .lock()
            .expect("auto-adoption tracker mutex poisoned")
    }
}

impl Default for AutoAdoptionTracker {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the auto-adoption pass over one heartbeat's stack list.
///
/// For every stack the agent reported:
///
/// 1. Look up the stack id by `(host_id, name)`. Skip if absent
///    (the sync_stacks pass runs first and inserts the row; a missed
///    insert is logged as a warning, not an auto-adopt trigger).
/// 2. Gather container ids belonging to that stack from
///    `containers_in_heartbeat`. This is the agent's `hb.containers`
///    slice pre-filtered to the right stack by the caller, expressed
///    as the operator-visible derived id (see `container_id::derive_container_id`).
/// 3. Look up the subset that has a rich snapshot row via
///    `rich_lookup`. The parallel agent + storage PR provides the
///    real implementation; until it lands, the heartbeat handler
///    passes a closure that always returns an empty vec, which keeps
///    every auto-adopt observation at [`Decision::MissingRichData`]
///    until rich data is real.
/// 4. Read `stacks.compose_yaml` to decide whether the stack already
///    has stored compose.
/// 5. Hand the observation to the tracker. On [`Decision::Synthesize`],
///    invoke `synth_and_write` (the bridge to the synthesizer module
///    + storage write that the parallel PR provides).
///
/// Decisions other than [`Decision::Synthesize`] are logged at
/// `debug` so a noisy heartbeat doesn't drown the controller log; the
/// `Synthesize` arm logs at `info` and emits the
/// `compose.auto_adopted` journal event.
///
/// Returns the per-stack decisions for the heartbeat so tests can
/// assert without scraping logs. Production callers can ignore the
/// return value.
pub async fn run_auto_adoption_pass<R, S, F1, F2>(
    tracker: &AutoAdoptionTracker,
    inventory: &isengard_storage::Inventory,
    host_id: HostId,
    stacks_in_heartbeat: &[(String, Vec<String>)],
    rich_lookup: R,
    synth_and_write: S,
) -> Vec<(String, Decision)>
where
    R: Fn(&[String]) -> F1,
    F1: std::future::Future<Output = Vec<String>>,
    S: Fn(StackId, String, Vec<String>) -> F2,
    F2: std::future::Future<Output = Result<(), String>>,
{
    let mut decisions = Vec::with_capacity(stacks_in_heartbeat.len());
    for (stack_name, container_ids) in stacks_in_heartbeat {
        let Some(stack_id) = lookup_stack_id_by_name(inventory, host_id, stack_name.as_str()).await
        else {
            tracing::debug!(
                stack = %stack_name,
                "auto-adopt: stack row not found, skipping (sync_stacks may not have inserted yet)"
            );
            continue;
        };

        let has_stored_compose = match inventory.get_stack_compose(stack_id).await {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    stack = %stack_name,
                    error = %e,
                    "auto-adopt: get_stack_compose failed, skipping",
                );
                continue;
            }
        };

        let rich_ids = rich_lookup(container_ids.as_slice()).await;
        let key = StackKey {
            host_id,
            stack_name: stack_name.clone(),
        };
        let observation = Observation {
            stack_key: key.clone(),
            stack_id,
            container_ids: container_ids.as_slice(),
            rich_container_ids: rich_ids.as_slice(),
            has_stored_compose,
        };
        let decision = tracker.observe(&observation);
        match &decision {
            Decision::Synthesize => {
                tracing::info!(
                    stack = %stack_name,
                    container_count = container_ids.len(),
                    "auto-adopt: stable heartbeats reached, synthesizing compose",
                );
                if let Err(e) =
                    synth_and_write(stack_id, stack_name.clone(), rich_ids.clone()).await
                {
                    tracing::warn!(
                        stack = %stack_name,
                        error = %e,
                        "auto-adopt: synth + write failed",
                    );
                }
            }
            other => {
                tracing::debug!(
                    stack = %stack_name,
                    decision = ?other,
                    "auto-adopt: observation result",
                );
            }
        }
        decisions.push((stack_name.clone(), decision));
    }
    decisions
}

/// Look up a stack id by `(host_id, name)`.
///
/// Used by [`run_auto_adoption_pass`] to translate the agent's
/// `StackInfo.name` into the surrogate id used by
/// `inventory.get_stack_compose` and `inventory.set_stack_compose`.
/// Returns `None` when the row hasn't been inserted yet (sync_stacks
/// is the first heartbeat-time write and may have failed on this
/// tick; the next tick's auto-adopt pass will see the row).
async fn lookup_stack_id_by_name(
    inventory: &isengard_storage::Inventory,
    host_id: HostId,
    name: &str,
) -> Option<StackId> {
    // No targeted `get_stack_by_name`; the cheapest path uses
    // list_stacks(host) and filters in memory. The list is bounded by
    // the number of stacks on one host (small in practice), so the
    // O(N) scan is fine.
    match inventory.list_stacks(Some(host_id)).await {
        Ok(rows) => rows.into_iter().find(|s| s.name == name).map(|s| s.id),
        Err(e) => {
            tracing::warn!(
                stack = %name,
                error = %e,
                "auto-adopt: list_stacks failed",
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use isengard_storage::host::HostId;

    fn key(name: &str) -> StackKey {
        StackKey {
            host_id: HostId::new(),
            stack_name: name.into(),
        }
    }

    fn obs<'a>(
        k: &StackKey,
        ids: &'a [String],
        rich: &'a [String],
        has_stored: bool,
    ) -> Observation<'a> {
        Observation {
            stack_key: k.clone(),
            stack_id: StackId(1),
            container_ids: ids,
            rich_container_ids: rich,
            has_stored_compose: has_stored,
        }
    }

    fn ids(slice: &[&str]) -> Vec<String> {
        slice.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn default_threshold_is_two() {
        let t = AutoAdoptionTracker::new();
        assert_eq!(t.heartbeats_to_stable(), DEFAULT_HEARTBEATS_TO_STABLE);
        assert_eq!(t.heartbeats_to_stable(), 2);
    }

    #[test]
    fn zero_threshold_rounds_up_to_one() {
        // Synthesize on first observation would defeat the debouncer
        // and contradict the spec; the constructor floors at 1.
        let t = AutoAdoptionTracker::with_threshold(0);
        assert_eq!(t.heartbeats_to_stable(), 1);
    }

    #[test]
    fn newly_tracked_then_stable_then_synthesize() {
        let t = AutoAdoptionTracker::new();
        let k = key("servarr");
        let cs = ids(&["c1", "c2", "c3"]);

        // First heartbeat: newly tracked.
        assert_eq!(t.observe(&obs(&k, &cs, &cs, false)), Decision::NewlyTracked);
        // Second heartbeat with the same set hits the floor (2) -> synthesize.
        assert_eq!(t.observe(&obs(&k, &cs, &cs, false)), Decision::Synthesize);
        // Third heartbeat short-circuits.
        assert_eq!(
            t.observe(&obs(&k, &cs, &cs, false)),
            Decision::AlreadyAdopted
        );
    }

    #[test]
    fn unstable_then_stable_resets_counter() {
        let t = AutoAdoptionTracker::new();
        let k = key("plex");
        let cs1 = ids(&["c1"]);
        let cs2 = ids(&["c1", "c2"]);
        let cs3 = ids(&["c1", "c2", "c3"]);

        assert_eq!(
            t.observe(&obs(&k, &cs1, &cs1, false)),
            Decision::NewlyTracked
        );
        assert_eq!(t.observe(&obs(&k, &cs2, &cs2, false)), Decision::SetChanged);
        assert_eq!(t.observe(&obs(&k, &cs3, &cs3, false)), Decision::SetChanged);
        // Stable after another identical heartbeat.
        assert_eq!(t.observe(&obs(&k, &cs3, &cs3, false)), Decision::Synthesize);
    }

    #[test]
    fn stable_but_not_yet_when_threshold_is_three() {
        let t = AutoAdoptionTracker::with_threshold(3);
        let k = key("media");
        let cs = ids(&["c1", "c2"]);

        assert_eq!(t.observe(&obs(&k, &cs, &cs, false)), Decision::NewlyTracked);
        assert_eq!(
            t.observe(&obs(&k, &cs, &cs, false)),
            Decision::StableButNotYet { stable_count: 2 }
        );
        assert_eq!(t.observe(&obs(&k, &cs, &cs, false)), Decision::Synthesize);
    }

    #[test]
    fn missing_rich_data_does_not_advance_counter() {
        let t = AutoAdoptionTracker::new();
        let k = key("traefik");
        let full = ids(&["c1", "c2", "c3"]);
        let partial = ids(&["c1", "c2"]);

        // Rich subset is short of the full set: skip.
        assert_eq!(
            t.observe(&obs(&k, &full, &partial, false)),
            Decision::MissingRichData
        );
        // Tracker recorded nothing.
        assert_eq!(t.tracked_stacks(), 0);

        // Now agent ships the third container's rich snapshot: start
        // pending fresh.
        assert_eq!(
            t.observe(&obs(&k, &full, &full, false)),
            Decision::NewlyTracked
        );
        assert_eq!(
            t.observe(&obs(&k, &full, &full, false)),
            Decision::Synthesize
        );
    }

    #[test]
    fn already_has_compose_short_circuits_and_sticks() {
        let t = AutoAdoptionTracker::new();
        let k = key("homepage");
        let cs = ids(&["c1"]);

        // Stored compose present: never auto-adopt.
        assert_eq!(
            t.observe(&obs(&k, &cs, &cs, true)),
            Decision::AlreadyHasCompose
        );
        // Subsequent heartbeats without stored compose still
        // short-circuit because the tracker stamped Adopted.
        assert_eq!(
            t.observe(&obs(&k, &cs, &cs, false)),
            Decision::AlreadyAdopted
        );
    }

    #[test]
    fn set_changed_after_stable_resets_and_re_stabilises() {
        let t = AutoAdoptionTracker::with_threshold(3);
        let k = key("compose-up");
        let cs1 = ids(&["c1", "c2"]);
        let cs2 = ids(&["c1", "c2", "c3"]);

        // Two heartbeats of cs1 (still short of the 3-floor).
        assert_eq!(
            t.observe(&obs(&k, &cs1, &cs1, false)),
            Decision::NewlyTracked
        );
        assert_eq!(
            t.observe(&obs(&k, &cs1, &cs1, false)),
            Decision::StableButNotYet { stable_count: 2 }
        );
        // Set changes: reset.
        assert_eq!(t.observe(&obs(&k, &cs2, &cs2, false)), Decision::SetChanged);
        // Climb from 1 to 3 again.
        assert_eq!(
            t.observe(&obs(&k, &cs2, &cs2, false)),
            Decision::StableButNotYet { stable_count: 2 }
        );
        assert_eq!(t.observe(&obs(&k, &cs2, &cs2, false)), Decision::Synthesize);
    }

    #[test]
    fn distinct_stacks_tracked_independently() {
        let t = AutoAdoptionTracker::new();
        let a = key("alpha");
        let b = key("beta");
        let ca = ids(&["a1"]);
        let cb = ids(&["b1", "b2"]);

        assert_eq!(t.observe(&obs(&a, &ca, &ca, false)), Decision::NewlyTracked);
        assert_eq!(t.observe(&obs(&b, &cb, &cb, false)), Decision::NewlyTracked);
        assert_eq!(t.tracked_stacks(), 2);

        // alpha adopts.
        assert_eq!(t.observe(&obs(&a, &ca, &ca, false)), Decision::Synthesize);
        // beta still pending; its counter advanced independently.
        assert_eq!(t.observe(&obs(&b, &cb, &cb, false)), Decision::Synthesize);
    }

    #[test]
    fn set_equality_is_order_insensitive() {
        let t = AutoAdoptionTracker::new();
        let k = key("order");
        let cs1 = ids(&["a", "b", "c"]);
        let cs2 = ids(&["c", "b", "a"]);

        assert_eq!(
            t.observe(&obs(&k, &cs1, &cs1, false)),
            Decision::NewlyTracked
        );
        // Re-ordered list is the same set: should hit synthesis, not
        // SetChanged.
        assert_eq!(t.observe(&obs(&k, &cs2, &cs2, false)), Decision::Synthesize);
    }

    /// Drives a 3-services, 4-heartbeats scenario end-to-end. Models
    /// the `docker compose up servarr` flow:
    ///
    /// - HB1: only `web` is up (depends_on hasn't satisfied yet).
    /// - HB2: `web` + `worker` (cache still starting).
    /// - HB3: `web` + `worker` + `cache`. First stable observation.
    /// - HB4: same set. Synthesis fires; the row tags as
    ///   `auto_synthesized`.
    ///
    /// This is the parametric replay of the spec's example sequence
    /// and the closest thing the controller-only test surface has to
    /// an integration test until the parallel rich-container PR lands.
    #[test]
    fn three_services_across_four_heartbeats_writes_on_heartbeat_four() {
        let t = AutoAdoptionTracker::new();
        let k = key("servarr");
        let web = ids(&["web"]);
        let web_worker = ids(&["web", "worker"]);
        let web_worker_cache = ids(&["web", "worker", "cache"]);

        // HB1: first observation of `web` alone.
        let d1 = t.observe(&obs(&k, &web, &web, false));
        assert_eq!(d1, Decision::NewlyTracked);
        // HB2: set grew (worker came up). Counter resets.
        let d2 = t.observe(&obs(&k, &web_worker, &web_worker, false));
        assert_eq!(d2, Decision::SetChanged);
        // HB3: set grew again (cache came up). Counter resets to 1.
        let d3 = t.observe(&obs(&k, &web_worker_cache, &web_worker_cache, false));
        assert_eq!(d3, Decision::SetChanged);
        // HB4: same set as HB3, counter ticks to 2 = threshold, FIRES.
        let d4 = t.observe(&obs(&k, &web_worker_cache, &web_worker_cache, false));
        assert_eq!(d4, Decision::Synthesize);
    }
}
