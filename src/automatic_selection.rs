use std::cmp::Ordering;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::controller::ConnectionsSnapshot;
use crate::storage::NodeQualityReadLease;

pub(crate) const CURRENT_SELECTOR_VIEW_ID: &str = "current-selector";
pub(crate) const STREAMING_VIEW_ID: &str = "streaming";
pub(crate) const ACTIVE_TRANSFER_WINDOW: Duration = Duration::from_secs(10);
pub(crate) const ACTIVE_TRANSFER_THRESHOLD_BYTES: u64 = 64 * 1024;
const ACTIVE_TRANSFER_STALE_AFTER: Duration = Duration::from_secs(3);

/// Stable identity for a node view. The string representation is deliberately the same shape
/// that future usability manifests will own, so adding a panel does not widen the selector API.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct NodeViewId(String);

impl NodeViewId {
    pub(crate) fn current_selector() -> Self {
        Self(CURRENT_SELECTOR_VIEW_ID.to_string())
    }

    pub(crate) fn streaming() -> Self {
        Self(STREAMING_VIEW_ID.to_string())
    }

    pub(crate) fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (!value.trim().is_empty() && value.trim() == value).then_some(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for NodeViewId {
    fn default() -> Self {
        Self::current_selector()
    }
}

impl fmt::Display for NodeViewId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl Serialize for NodeViewId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for NodeViewId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).ok_or_else(|| serde::de::Error::custom("node view id must be non-empty"))
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RankingPolicy {
    #[default]
    Balanced,
    LowLatency,
    Throughput,
}

impl RankingPolicy {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Balanced => "balanced",
            Self::LowLatency => "low latency",
            Self::Throughput => "throughput",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PanelMembership {
    Included,
    Rejected,
    Untested,
    Incomplete,
    // Dynamic manifests in #18 can project an expired fact without widening this decision API.
    #[allow(dead_code)]
    Expired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeViewProjection {
    pub(crate) id: NodeViewId,
    pub(crate) label: String,
    pub(crate) ranking_policy: RankingPolicy,
    pub(crate) members: BTreeMap<String, PanelMembership>,
}

impl NodeViewProjection {
    #[cfg(test)]
    pub(crate) fn current_selector(members: &[String]) -> Self {
        Self {
            id: NodeViewId::current_selector(),
            label: "Current selector".to_string(),
            ranking_policy: RankingPolicy::Balanced,
            members: members
                .iter()
                .cloned()
                .map(|node| (node, PanelMembership::Included))
                .collect(),
        }
    }

    pub(crate) fn membership(&self, node: &str) -> PanelMembership {
        self.members
            .get(node)
            .copied()
            .unwrap_or(PanelMembership::Untested)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum ReachabilityTier {
    Unreachable,
    Degraded,
    Reachable,
    StableReachable,
}

impl ReachabilityTier {
    pub(crate) fn from_successes(successes: u8) -> Self {
        match successes.min(3) {
            3 => Self::StableReachable,
            2 => Self::Reachable,
            1 => Self::Degraded,
            _ => Self::Unreachable,
        }
    }

    pub(crate) fn successes(self) -> u8 {
        match self {
            Self::StableReachable => 3,
            Self::Reachable => 2,
            Self::Degraded => 1,
            Self::Unreachable => 0,
        }
    }

    fn eligible(self) -> bool {
        self >= Self::Reachable
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeQualityFacts {
    pub(crate) node: String,
    pub(crate) reachability: Option<ReachabilityTier>,
    pub(crate) recent_quick_successes: usize,
    pub(crate) recent_quick_rounds: usize,
    pub(crate) warm_median_ms: Option<u64>,
    pub(crate) p95_ms: Option<u64>,
    pub(crate) cold_start_ms: Option<u64>,
    pub(crate) sustained_successes: usize,
    pub(crate) sustained_attempts: usize,
    pub(crate) throughput_bytes_per_second: Option<u64>,
    pub(crate) config_order: usize,
}

impl NodeQualityFacts {
    fn is_eligible(&self, policy: RankingPolicy) -> bool {
        self.reachability.is_some_and(ReachabilityTier::eligible)
            && (!matches!(policy, RankingPolicy::Throughput)
                || self.throughput_bytes_per_second.is_some())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SelectionScope {
    pub(crate) quality_generation: u64,
    pub(crate) selector: String,
    pub(crate) panel: NodeViewId,
    pub(crate) current_node: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ActiveNodeTransfer {
    Idle { growth_bytes: u64 },
    Active { growth_bytes: u64 },
    Warming { observed_millis: u64 },
    Unavailable { detail: String },
}

#[derive(Default)]
pub(crate) struct ActiveNodeTrafficTracker {
    scope: Option<SelectionScope>,
    baseline_at: Option<Instant>,
    last_observed_at: Option<Instant>,
    connection_totals: BTreeMap<String, u64>,
    growth: VecDeque<(Instant, u64)>,
    unavailable: Option<String>,
}

impl ActiveNodeTrafficTracker {
    pub(crate) fn observe(
        &mut self,
        scope: SelectionScope,
        observed_at: Instant,
        snapshot: &ConnectionsSnapshot,
    ) {
        if self.scope.as_ref() != Some(&scope) || self.baseline_at.is_none() {
            self.reset_to_baseline(scope, observed_at, snapshot);
            return;
        }

        let mut positive_growth = 0_u64;
        let mut observed_totals = BTreeMap::new();
        // Controller totals are lifetime counters. The rolling 10-second guard must therefore use
        // positive deltas per real connection ID and exact chain element, never global traffic.
        for connection in snapshot.connections.iter().filter(|connection| {
            connection
                .chains
                .iter()
                .any(|chain| chain == &scope.current_node)
        }) {
            let total = connection.upload.saturating_add(connection.download);
            let delta = self
                .connection_totals
                .get(&connection.id)
                .map_or(total, |previous| {
                    if total >= *previous {
                        total - *previous
                    } else {
                        // A controller counter reset is a new monotonic segment, not negative
                        // traffic. Counting the new segment preserves only observed growth.
                        total
                    }
                });
            positive_growth = positive_growth.saturating_add(delta);
            observed_totals.insert(connection.id.clone(), total);
        }
        // Only exact IDs on the current node in this snapshot remain baselines. Dropping vanished
        // IDs bounds a lifetime worker's memory and makes a later reappearance a fresh monotonic
        // segment, while the disappearance itself still contributes no negative traffic.
        self.connection_totals = observed_totals;
        if positive_growth > 0 {
            self.growth.push_back((observed_at, positive_growth));
        }
        self.last_observed_at = Some(observed_at);
        self.unavailable = None;
        self.prune(observed_at);
    }

    pub(crate) fn mark_unavailable(&mut self, detail: impl Into<String>) {
        self.scope = None;
        self.baseline_at = None;
        self.last_observed_at = None;
        self.connection_totals.clear();
        self.growth.clear();
        self.unavailable = Some(detail.into());
    }

    pub(crate) fn status(&mut self, scope: &SelectionScope, now: Instant) -> ActiveNodeTransfer {
        if let Some(detail) = &self.unavailable {
            return ActiveNodeTransfer::Unavailable {
                detail: detail.clone(),
            };
        }
        let Some(baseline_at) = self
            .baseline_at
            .filter(|_| self.scope.as_ref() == Some(scope))
        else {
            return ActiveNodeTransfer::Warming { observed_millis: 0 };
        };
        if self.last_observed_at.is_none_or(|observed| {
            now.saturating_duration_since(observed) > ACTIVE_TRANSFER_STALE_AFTER
        }) {
            return ActiveNodeTransfer::Unavailable {
                detail: "connection snapshot is stale".to_string(),
            };
        }
        let observed = now.saturating_duration_since(baseline_at);
        if observed < ACTIVE_TRANSFER_WINDOW {
            return ActiveNodeTransfer::Warming {
                observed_millis: observed.as_millis() as u64,
            };
        }
        self.prune(now);
        let growth_bytes = self
            .growth
            .iter()
            .fold(0_u64, |total, (_, delta)| total.saturating_add(*delta));
        if growth_bytes > ACTIVE_TRANSFER_THRESHOLD_BYTES {
            ActiveNodeTransfer::Active { growth_bytes }
        } else {
            ActiveNodeTransfer::Idle { growth_bytes }
        }
    }

    fn reset_to_baseline(
        &mut self,
        scope: SelectionScope,
        observed_at: Instant,
        snapshot: &ConnectionsSnapshot,
    ) {
        self.connection_totals = snapshot
            .connections
            .iter()
            .filter(|connection| {
                connection
                    .chains
                    .iter()
                    .any(|chain| chain == &scope.current_node)
            })
            .map(|connection| {
                (
                    connection.id.clone(),
                    connection.upload.saturating_add(connection.download),
                )
            })
            .collect();
        self.scope = Some(scope);
        self.baseline_at = Some(observed_at);
        self.last_observed_at = Some(observed_at);
        self.growth.clear();
        self.unavailable = None;
    }

    fn prune(&mut self, now: Instant) {
        while self
            .growth
            .front()
            .is_some_and(|(at, _)| now.saturating_duration_since(*at) > ACTIVE_TRANSFER_WINDOW)
        {
            self.growth.pop_front();
        }
    }

    #[cfg(test)]
    fn tracked_connection_count(&self) -> usize {
        self.connection_totals.len()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AutoSelectionReason {
    AwaitingConfirmation {
        candidate: String,
        wins: u8,
        required: u8,
    },
    SwitchConfirmed {
        candidate: String,
    },
    RouteActivationAwaiting {
        current: String,
        wins: u8,
        required: u8,
    },
    RouteActivationConfirmed {
        current: String,
    },
    EmergencyAwaitingConfirmation {
        candidate: String,
        failures: u8,
        required: u8,
    },
    EmergencySwitch {
        candidate: String,
    },
    CurrentPreferred,
    NoEligibleCandidate,
    IncompleteCurrentAssessment,
    IncompletePanelAssessment,
    IncompleteImprovementEvidence,
    DuplicateRound {
        round_id: u64,
    },
    QualityFactsUnavailable {
        detail: String,
    },
    InsufficientImprovement {
        candidate: String,
    },
    ActiveTransfer {
        growth_bytes: u64,
    },
    TransferWindowWarming {
        observed_millis: u64,
    },
    TransferUnavailable {
        detail: String,
    },
    ReachabilityTrafficAnomaly {
        growth_bytes: u64,
    },
}

impl AutoSelectionReason {
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::AwaitingConfirmation {
                candidate,
                wins,
                required,
            } => format!("{candidate} leads; awaiting confirmation {wins}/{required}"),
            Self::SwitchConfirmed { candidate } => {
                format!("{candidate} won two complete rounds with material improvement")
            }
            Self::RouteActivationAwaiting {
                current,
                wins,
                required,
            } => format!(
                "{current} remains preferred; awaiting route confirmation {wins}/{required}"
            ),
            Self::RouteActivationConfirmed { current } => format!(
                "{current} remained preferred for two complete rounds; route activation confirmed"
            ),
            Self::EmergencyAwaitingConfirmation {
                candidate,
                failures,
                required,
            } => format!(
                "current node is 0/3 without traffic; {candidate} is eligible, emergency confirmation {failures}/{required}"
            ),
            Self::EmergencySwitch { candidate } => format!(
                "emergency switch to {candidate}: current node was 0/3 without traffic twice"
            ),
            Self::CurrentPreferred => "current node remains the highest-ranked candidate".into(),
            Self::NoEligibleCandidate => {
                "active panel has no candidate with at least 2/3 reachability".into()
            }
            Self::IncompleteCurrentAssessment => {
                "current node does not have one complete three-attempt assessment".into()
            }
            Self::IncompletePanelAssessment => {
                "active panel still has incomplete or untested candidate evidence".into()
            }
            Self::IncompleteImprovementEvidence => {
                "material-improvement evidence is incomplete for the current ranking policy".into()
            }
            Self::DuplicateRound { round_id } => {
                format!("completed assessment round {round_id} was already evaluated")
            }
            Self::QualityFactsUnavailable { detail } => {
                format!("node-quality facts changed before selection ({detail})")
            }
            Self::InsufficientImprovement { candidate } => format!(
                "{candidate} leads but does not improve the current node by about 20% in the same reachability tier"
            ),
            Self::ActiveTransfer { growth_bytes } => format!(
                "switch deferred: current-node connections grew by {growth_bytes} bytes in 10s"
            ),
            Self::TransferWindowWarming { observed_millis } => format!(
                "switch deferred: current-node traffic window is warming ({observed_millis}/10000ms)"
            ),
            Self::TransferUnavailable { detail } => {
                format!("switch deferred: current-node traffic is unavailable ({detail})")
            }
            Self::ReachabilityTrafficAnomaly { growth_bytes } => format!(
                "switch deferred: current node measured 0/3 while its connections grew by {growth_bytes} bytes"
            ),
        }
    }
}

const MAX_EXPLANATION_CHARS: usize = 512;

#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
pub(crate) struct AutoSelectionExplanation {
    pub(crate) selector: String,
    pub(crate) panel: NodeViewId,
    pub(crate) detail: String,
}

impl AutoSelectionExplanation {
    pub(crate) fn new(
        selector: impl Into<String>,
        panel: NodeViewId,
        reason: &AutoSelectionReason,
    ) -> Self {
        let rendered = reason.detail();
        let truncated = rendered.chars().count() > MAX_EXPLANATION_CHARS;
        let mut detail = rendered
            .chars()
            .take(MAX_EXPLANATION_CHARS - usize::from(truncated))
            .collect::<String>();
        if truncated {
            detail.push('…');
        }
        Self {
            selector: selector.into(),
            panel,
            detail,
        }
    }

    pub(crate) fn matches(&self, selector: &str, panel: &NodeViewId) -> bool {
        self.selector == selector && &self.panel == panel
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AutoSelectionDecision {
    pub(crate) target_node: Option<String>,
    pub(crate) activate_route: bool,
    pub(crate) reason: AutoSelectionReason,
}

pub(crate) struct AutoSelectionPlan {
    pub(crate) decision: AutoSelectionDecision,
    pub(crate) parent_switch: Option<(String, String)>,
    _quality_lease: NodeQualityReadLease,
}

impl AutoSelectionPlan {
    pub(crate) fn new(
        decision: AutoSelectionDecision,
        parent_switch: Option<(String, String)>,
        quality_lease: NodeQualityReadLease,
    ) -> Self {
        Self {
            decision,
            parent_switch,
            _quality_lease: quality_lease,
        }
    }
}

#[derive(Default)]
pub(crate) struct AutomaticSelectionState {
    scope: Option<SelectionScope>,
    pending_candidate: Option<String>,
    pending_wins: u8,
    emergency_failures: u8,
    last_round_id: Option<u64>,
}

impl AutomaticSelectionState {
    pub(crate) fn evaluate(
        &mut self,
        scope: SelectionScope,
        round_id: u64,
        route_activation_required: bool,
        panel: &NodeViewProjection,
        facts: &[NodeQualityFacts],
        transfer: ActiveNodeTransfer,
    ) -> AutoSelectionDecision {
        if self.scope.as_ref() != Some(&scope) {
            self.reset_for_scope(scope.clone());
        }
        // A completion may be replayed or arrive out of order across UI/background boundaries.
        // Monotonic immutable IDs prevent any old three-attempt assessment from satisfying a new
        // confirmation round.
        if self
            .last_round_id
            .is_some_and(|last_round_id| round_id <= last_round_id)
        {
            return decision(AutoSelectionReason::DuplicateRound { round_id });
        }
        self.last_round_id = Some(round_id);

        let current = facts.iter().find(|facts| facts.node == scope.current_node);
        let Some(current) = current else {
            self.reset_rounds();
            return decision(AutoSelectionReason::IncompleteCurrentAssessment);
        };
        let Some(current_tier) = current.reachability else {
            self.reset_rounds();
            return decision(AutoSelectionReason::IncompleteCurrentAssessment);
        };

        if panel.members.iter().any(|(node, membership)| {
            *membership == PanelMembership::Included
                && facts
                    .iter()
                    .find(|facts| facts.node == *node)
                    .is_none_or(|facts| facts.reachability.is_none())
        }) {
            self.reset_rounds();
            return decision(AutoSelectionReason::IncompletePanelAssessment);
        }

        let eligible = facts
            .iter()
            .filter(|facts| panel.membership(&facts.node) == PanelMembership::Included)
            .filter(|facts| facts.is_eligible(panel.ranking_policy))
            .collect::<Vec<_>>();
        let best = eligible
            .into_iter()
            .max_by(|left, right| compare_candidates(panel.ranking_policy, left, right));
        let Some(best) = best else {
            self.reset_rounds();
            let incomplete = panel.members.values().any(|membership| {
                matches!(
                    membership,
                    PanelMembership::Untested
                        | PanelMembership::Incomplete
                        | PanelMembership::Expired
                )
            });
            return decision(if incomplete {
                AutoSelectionReason::IncompletePanelAssessment
            } else {
                AutoSelectionReason::NoEligibleCandidate
            });
        };

        if current_tier == ReachabilityTier::Unreachable {
            self.pending_candidate = None;
            self.pending_wins = 0;
            return self.evaluate_emergency(best, transfer);
        }
        self.emergency_failures = 0;

        match transfer {
            ActiveNodeTransfer::Active { growth_bytes } => {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::ActiveTransfer { growth_bytes });
            }
            ActiveNodeTransfer::Warming { observed_millis } => {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::TransferWindowWarming { observed_millis });
            }
            ActiveNodeTransfer::Unavailable { detail } => {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::TransferUnavailable { detail });
            }
            ActiveNodeTransfer::Idle { .. } => {}
        }

        if best.node == current.node {
            if !route_activation_required {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::CurrentPreferred);
            }
            return self.confirm_candidate(best, true);
        }
        match material_improvement(panel, current, best) {
            Some(true) => {}
            Some(false) => {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::InsufficientImprovement {
                    candidate: best.node.clone(),
                });
            }
            None => {
                self.pending_candidate = None;
                self.pending_wins = 0;
                return decision(AutoSelectionReason::IncompleteImprovementEvidence);
            }
        }

        self.confirm_candidate(best, false)
    }

    fn confirm_candidate(
        &mut self,
        best: &NodeQualityFacts,
        route_only: bool,
    ) -> AutoSelectionDecision {
        // Candidate identity is part of the confirmation, not just the fact that somebody won.
        // Otherwise alternating nodes could collectively satisfy the two-round safety gate.
        if self.pending_candidate.as_deref() == Some(best.node.as_str()) {
            self.pending_wins = self.pending_wins.saturating_add(1);
        } else {
            self.pending_candidate = Some(best.node.clone());
            self.pending_wins = 1;
        }
        if self.pending_wins < 2 {
            return decision(if route_only {
                AutoSelectionReason::RouteActivationAwaiting {
                    current: best.node.clone(),
                    wins: self.pending_wins,
                    required: 2,
                }
            } else {
                AutoSelectionReason::AwaitingConfirmation {
                    candidate: best.node.clone(),
                    wins: self.pending_wins,
                    required: 2,
                }
            });
        }

        self.pending_candidate = None;
        self.pending_wins = 0;
        AutoSelectionDecision {
            target_node: (!route_only).then(|| best.node.clone()),
            activate_route: true,
            reason: if route_only {
                AutoSelectionReason::RouteActivationConfirmed {
                    current: best.node.clone(),
                }
            } else {
                AutoSelectionReason::SwitchConfirmed {
                    candidate: best.node.clone(),
                }
            },
        }
    }

    fn evaluate_emergency(
        &mut self,
        best: &NodeQualityFacts,
        transfer: ActiveNodeTransfer,
    ) -> AutoSelectionDecision {
        match transfer {
            ActiveNodeTransfer::Idle { growth_bytes: 0 } => {
                // Normal confirmation is candidate-bound, but emergency confirmation is
                // outage-bound: two consecutive idle 0/3 rounds prove the current node failed;
                // the second round's best eligible node is the safest recovery target then.
                self.emergency_failures = self.emergency_failures.saturating_add(1);
                if self.emergency_failures < 2 {
                    return decision(AutoSelectionReason::EmergencyAwaitingConfirmation {
                        candidate: best.node.clone(),
                        failures: self.emergency_failures,
                        required: 2,
                    });
                }
                self.emergency_failures = 0;
                AutoSelectionDecision {
                    target_node: Some(best.node.clone()),
                    activate_route: true,
                    reason: AutoSelectionReason::EmergencySwitch {
                        candidate: best.node.clone(),
                    },
                }
            }
            ActiveNodeTransfer::Idle { growth_bytes } => {
                // Normal switching tolerates up to 64 KiB/10s, but emergency evidence requires
                // literal zero growth: any positive counter delta contradicts a 0/3 outage.
                self.emergency_failures = 0;
                decision(AutoSelectionReason::ReachabilityTrafficAnomaly { growth_bytes })
            }
            ActiveNodeTransfer::Active { growth_bytes } => {
                self.emergency_failures = 0;
                decision(AutoSelectionReason::ReachabilityTrafficAnomaly { growth_bytes })
            }
            ActiveNodeTransfer::Warming { observed_millis } => {
                self.emergency_failures = 0;
                decision(AutoSelectionReason::TransferWindowWarming { observed_millis })
            }
            ActiveNodeTransfer::Unavailable { detail } => {
                self.emergency_failures = 0;
                decision(AutoSelectionReason::TransferUnavailable { detail })
            }
        }
    }

    fn reset_for_scope(&mut self, scope: SelectionScope) {
        self.scope = Some(scope);
        self.last_round_id = None;
        self.reset_rounds();
    }

    fn reset_rounds(&mut self) {
        self.pending_candidate = None;
        self.pending_wins = 0;
        self.emergency_failures = 0;
    }
}

fn decision(reason: AutoSelectionReason) -> AutoSelectionDecision {
    AutoSelectionDecision {
        target_node: None,
        activate_route: false,
        reason,
    }
}

fn compare_candidates(
    policy: RankingPolicy,
    left: &NodeQualityFacts,
    right: &NodeQualityFacts,
) -> Ordering {
    left.reachability
        .cmp(&right.reachability)
        .then_with(|| match policy {
            RankingPolicy::Balanced => compare_ratio_higher(
                left.recent_quick_successes,
                left.recent_quick_rounds,
                right.recent_quick_successes,
                right.recent_quick_rounds,
            )
            .then_with(|| compare_optional_lower(left.warm_median_ms, right.warm_median_ms))
            .then_with(|| compare_optional_lower(left.p95_ms, right.p95_ms))
            .then_with(|| {
                left.throughput_bytes_per_second
                    .is_some()
                    .cmp(&right.throughput_bytes_per_second.is_some())
            })
            .then_with(|| {
                left.throughput_bytes_per_second
                    .cmp(&right.throughput_bytes_per_second)
            }),
            RankingPolicy::LowLatency => {
                compare_optional_lower(left.warm_median_ms, right.warm_median_ms)
                    .then_with(|| compare_optional_lower(left.p95_ms, right.p95_ms))
                    .then_with(|| compare_optional_lower(left.cold_start_ms, right.cold_start_ms))
                    .then_with(|| {
                        compare_ratio_higher(
                            left.recent_quick_successes,
                            left.recent_quick_rounds,
                            right.recent_quick_successes,
                            right.recent_quick_rounds,
                        )
                    })
            }
            RankingPolicy::Throughput => left
                .throughput_bytes_per_second
                .cmp(&right.throughput_bytes_per_second)
                .then_with(|| {
                    compare_ratio_higher(
                        left.sustained_successes,
                        left.sustained_attempts,
                        right.sustained_successes,
                        right.sustained_attempts,
                    )
                })
                .then_with(|| compare_optional_lower(left.p95_ms, right.p95_ms))
                .then_with(|| compare_optional_lower(left.cold_start_ms, right.cold_start_ms)),
        })
        // Lower configuration order wins the final deterministic tie-break.
        .then_with(|| right.config_order.cmp(&left.config_order))
}

fn compare_ratio_higher(
    left_successes: usize,
    left_attempts: usize,
    right_successes: usize,
    right_attempts: usize,
) -> Ordering {
    let left_attempts = left_attempts.max(1) as u128;
    let right_attempts = right_attempts.max(1) as u128;
    (left_successes as u128 * right_attempts).cmp(&(right_successes as u128 * left_attempts))
}

fn compare_optional_lower(left: Option<u64>, right: Option<u64>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => right.cmp(&left),
        (Some(_), None) => Ordering::Greater,
        (None, Some(_)) => Ordering::Less,
        (None, None) => Ordering::Equal,
    }
}

fn material_improvement(
    panel: &NodeViewProjection,
    current: &NodeQualityFacts,
    candidate: &NodeQualityFacts,
) -> Option<bool> {
    let current_tier = current
        .reachability
        .expect("material comparison requires complete current reachability");
    let candidate_tier = candidate
        .reachability
        .expect("eligible candidate has complete reachability");
    if candidate_tier != current_tier {
        return Some(candidate_tier > current_tier);
    }
    if panel.membership(&current.node) != PanelMembership::Included {
        return Some(true);
    }

    // Cross multiplication makes the 20% gate exact and deterministic, without float rounding or
    // overflow from large controller counters.
    match panel.ranking_policy {
        RankingPolicy::Balanced | RankingPolicy::LowLatency => candidate
            .warm_median_ms
            .zip(current.warm_median_ms)
            .map(|(candidate, current)| candidate as u128 * 5 <= current as u128 * 4),
        RankingPolicy::Throughput => candidate
            .throughput_bytes_per_second
            .zip(current.throughput_bytes_per_second)
            .map(|(candidate, current)| candidate as u128 * 5 >= current as u128 * 6),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::controller::{ConnectionInfo, ConnectionMetadata, ConnectionsSnapshot};

    fn facts(node: &str, reachability: u8, warm_ms: u64, order: usize) -> NodeQualityFacts {
        NodeQualityFacts {
            node: node.to_string(),
            reachability: Some(ReachabilityTier::from_successes(reachability)),
            recent_quick_successes: 3,
            recent_quick_rounds: 3,
            warm_median_ms: Some(warm_ms),
            p95_ms: Some(warm_ms + 10),
            cold_start_ms: Some(warm_ms + 20),
            sustained_successes: 3,
            sustained_attempts: 3,
            throughput_bytes_per_second: Some(1_000_000),
            config_order: order,
        }
    }

    fn scope(current: &str) -> SelectionScope {
        SelectionScope {
            quality_generation: 7,
            selector: "select".to_string(),
            panel: NodeViewId::current_selector(),
            current_node: current.to_string(),
        }
    }

    fn idle() -> ActiveNodeTransfer {
        ActiveNodeTransfer::Idle { growth_bytes: 0 }
    }

    fn connections(rows: &[(&str, u64, u64, &[&str])]) -> ConnectionsSnapshot {
        ConnectionsSnapshot {
            connections: rows
                .iter()
                .map(|(id, upload, download, chains)| ConnectionInfo {
                    id: (*id).to_string(),
                    upload: *upload,
                    download: *download,
                    start: None,
                    chains: chains.iter().map(|chain| (*chain).to_string()).collect(),
                    rule: None,
                    rule_payload: None,
                    metadata: ConnectionMetadata::default(),
                })
                .collect(),
            ..ConnectionsSnapshot::default()
        }
    }

    #[test]
    fn stable_node_view_id_round_trips_as_one_string() {
        let dynamic = NodeViewId::new("agy-gemini").unwrap();
        let encoded = serde_json::to_string(&dynamic).unwrap();
        assert_eq!(encoded, "\"agy-gemini\"");
        assert_eq!(
            serde_json::from_str::<NodeViewId>(&encoded).unwrap(),
            dynamic
        );
        assert!(serde_json::from_str::<NodeViewId>("\" \"").is_err());
    }

    #[test]
    fn exported_explanation_is_bounded_for_background_status() {
        let explanation = AutoSelectionExplanation::new(
            "select",
            NodeViewId::current_selector(),
            &AutoSelectionReason::QualityFactsUnavailable {
                detail: "x".repeat(2_000),
            },
        );

        assert_eq!(explanation.detail.chars().count(), MAX_EXPLANATION_CHARS);
        assert!(explanation.detail.ends_with('…'));
    }

    #[test]
    fn normal_switch_requires_same_materially_better_candidate_twice() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let rows = [facts("current", 3, 100, 0), facts("candidate", 3, 80, 1)];
        let mut state = AutomaticSelectionState::default();

        let first = state.evaluate(scope("current"), 1, false, &panel, &rows, idle());
        assert_eq!(first.target_node, None);
        assert!(matches!(
            first.reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));
        let duplicate = state.evaluate(scope("current"), 1, false, &panel, &rows, idle());
        assert!(matches!(
            duplicate.reason,
            AutoSelectionReason::DuplicateRound { round_id: 1 }
        ));
        let second = state.evaluate(scope("current"), 2, false, &panel, &rows, idle());
        assert_eq!(second.target_node.as_deref(), Some("candidate"));
        assert!(matches!(
            second.reason,
            AutoSelectionReason::SwitchConfirmed { .. }
        ));
    }

    #[test]
    fn an_older_completed_round_cannot_be_replayed_as_fresh_confirmation() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let rows = [facts("current", 3, 100, 0), facts("candidate", 3, 80, 1)];
        let mut state = AutomaticSelectionState::default();

        let first = state.evaluate(scope("current"), 2, false, &panel, &rows, idle());
        assert!(matches!(
            first.reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));
        let replay = state.evaluate(scope("current"), 1, false, &panel, &rows, idle());
        assert!(matches!(
            replay.reason,
            AutoSelectionReason::DuplicateRound { round_id: 1 }
        ));
        let fresh = state.evaluate(scope("current"), 3, false, &panel, &rows, idle());
        assert_eq!(fresh.target_node.as_deref(), Some("candidate"));
    }

    #[test]
    fn exact_twenty_percent_is_material_but_one_millisecond_slower_is_not() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let current = facts("current", 3, 100, 0);
        let exact = facts("candidate", 3, 80, 1);
        let close = facts("candidate", 3, 81, 1);
        assert_eq!(material_improvement(&panel, &current, &exact), Some(true));
        assert_eq!(material_improvement(&panel, &current, &close), Some(false));

        let mut missing_warm = exact;
        missing_warm.warm_median_ms = None;
        assert_eq!(material_improvement(&panel, &current, &missing_warm), None);
    }

    #[test]
    fn a_higher_reachability_tier_is_categorical_but_still_needs_two_wins() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let rows = [facts("current", 2, 50, 0), facts("candidate", 3, 500, 1)];
        let mut state = AutomaticSelectionState::default();
        assert!(matches!(
            state
                .evaluate(scope("current"), 1, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::AwaitingConfirmation { .. }
        ));
        assert_eq!(
            state
                .evaluate(scope("current"), 2, false, &panel, &rows, idle())
                .target_node
                .as_deref(),
            Some("candidate")
        );
    }

    #[test]
    fn candidate_change_and_scope_change_reset_confirmation() {
        let panel = NodeViewProjection::current_selector(&[
            "current".into(),
            "candidate-a".into(),
            "candidate-b".into(),
        ]);
        let mut state = AutomaticSelectionState::default();
        let first = [
            facts("current", 3, 100, 0),
            facts("candidate-a", 3, 70, 1),
            facts("candidate-b", 3, 90, 2),
        ];
        let second = [
            facts("current", 3, 100, 0),
            facts("candidate-a", 3, 95, 1),
            facts("candidate-b", 3, 60, 2),
        ];
        state.evaluate(scope("current"), 1, false, &panel, &first, idle());
        let changed = state.evaluate(scope("current"), 2, false, &panel, &second, idle());
        assert!(matches!(
            changed.reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));

        let mut new_scope = scope("current");
        new_scope.quality_generation += 1;
        let reset = state.evaluate(new_scope, 3, false, &panel, &second, idle());
        assert!(matches!(
            reset.reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));
    }

    #[test]
    fn transfer_and_incomplete_evidence_defer_and_break_confirmation() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let mut rows = [facts("current", 3, 100, 0), facts("candidate", 3, 70, 1)];
        let mut state = AutomaticSelectionState::default();
        state.evaluate(scope("current"), 1, false, &panel, &rows, idle());
        let active = state.evaluate(
            scope("current"),
            2,
            false,
            &panel,
            &rows,
            ActiveNodeTransfer::Active {
                growth_bytes: 65_537,
            },
        );
        assert!(matches!(
            active.reason,
            AutoSelectionReason::ActiveTransfer { .. }
        ));
        assert!(matches!(
            state
                .evaluate(scope("current"), 3, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));

        rows[0].reachability = None;
        assert_eq!(
            state
                .evaluate(scope("current"), 4, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::IncompleteCurrentAssessment
        );
    }

    #[test]
    fn emergency_requires_two_zero_of_three_idle_rounds_and_candidate_two_of_three() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let mut state = AutomaticSelectionState::default();
        let rows = [facts("current", 0, 100, 0), facts("candidate", 2, 500, 1)];
        assert!(matches!(
            state
                .evaluate(scope("current"), 1, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::EmergencyAwaitingConfirmation { failures: 1, .. }
        ));
        assert!(matches!(
            state
                .evaluate(scope("current"), 2, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::EmergencySwitch { .. }
        ));

        let ineligible = [facts("current", 0, 100, 0), facts("candidate", 1, 50, 1)];
        let mut state = AutomaticSelectionState::default();
        assert_eq!(
            state
                .evaluate(scope("current"), 1, false, &panel, &ineligible, idle())
                .reason,
            AutoSelectionReason::NoEligibleCandidate
        );

        let panel = NodeViewProjection::current_selector(&[
            "current".into(),
            "candidate-a".into(),
            "candidate-b".into(),
        ]);
        let first = [
            facts("current", 0, 100, 0),
            facts("candidate-a", 3, 50, 1),
            facts("candidate-b", 3, 100, 2),
        ];
        let second = [
            facts("current", 0, 100, 0),
            facts("candidate-a", 3, 100, 1),
            facts("candidate-b", 3, 50, 2),
        ];
        let mut state = AutomaticSelectionState::default();
        state.evaluate(scope("current"), 1, false, &panel, &first, idle());
        let recovery = state.evaluate(scope("current"), 2, false, &panel, &second, idle());
        assert_eq!(recovery.target_node.as_deref(), Some("candidate-b"));
        assert!(matches!(
            recovery.reason,
            AutoSelectionReason::EmergencySwitch { candidate }
                if candidate == "candidate-b"
        ));
    }

    #[test]
    fn active_traffic_turns_zero_of_three_into_anomaly_not_emergency_evidence() {
        let panel = NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        let rows = [facts("current", 0, 100, 0), facts("candidate", 3, 70, 1)];
        let mut state = AutomaticSelectionState::default();
        let anomaly = state.evaluate(
            scope("current"),
            1,
            false,
            &panel,
            &rows,
            ActiveNodeTransfer::Active {
                growth_bytes: 90_000,
            },
        );
        assert!(matches!(
            anomaly.reason,
            AutoSelectionReason::ReachabilityTrafficAnomaly { .. }
        ));
        assert!(matches!(
            state
                .evaluate(scope("current"), 2, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::EmergencyAwaitingConfirmation { failures: 1, .. }
        ));
    }

    #[test]
    fn throughput_policy_uses_exact_twenty_percent_gate() {
        let mut panel =
            NodeViewProjection::current_selector(&["current".into(), "candidate".into()]);
        panel.ranking_policy = RankingPolicy::Throughput;
        let mut current = facts("current", 3, 100, 0);
        current.throughput_bytes_per_second = Some(1_000);
        let mut candidate = facts("candidate", 3, 100, 1);
        candidate.throughput_bytes_per_second = Some(1_200);
        assert_eq!(
            material_improvement(&panel, &current, &candidate),
            Some(true)
        );
        candidate.throughput_bytes_per_second = Some(1_199);
        assert_eq!(
            material_improvement(&panel, &current, &candidate),
            Some(false)
        );
    }

    #[test]
    fn panel_membership_is_a_hard_candidate_boundary() {
        let mut panel =
            NodeViewProjection::current_selector(&["current".into(), "included".into()]);
        panel
            .members
            .insert("rejected".into(), PanelMembership::Rejected);
        let rows = [
            facts("current", 3, 100, 0),
            facts("included", 3, 80, 1),
            facts("rejected", 3, 10, 2),
        ];
        let mut state = AutomaticSelectionState::default();
        assert!(matches!(
            state
                .evaluate(scope("current"), 1, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::AwaitingConfirmation { candidate, .. } if candidate == "included"
        ));
    }

    #[test]
    fn current_outside_active_panel_is_a_categorical_material_change() {
        let mut panel = NodeViewProjection::current_selector(&["candidate".into()]);
        panel
            .members
            .insert("current".into(), PanelMembership::Rejected);
        let rows = [facts("current", 3, 20, 0), facts("candidate", 3, 500, 1)];
        let mut state = AutomaticSelectionState::default();

        assert!(matches!(
            state
                .evaluate(scope("current"), 1, false, &panel, &rows, idle())
                .reason,
            AutoSelectionReason::AwaitingConfirmation { wins: 1, .. }
        ));
        assert_eq!(
            state
                .evaluate(scope("current"), 2, false, &panel, &rows, idle())
                .target_node
                .as_deref(),
            Some("candidate")
        );
    }

    #[test]
    fn implicit_route_activation_also_requires_two_distinct_rounds() {
        let panel = NodeViewProjection::current_selector(&["current".into()]);
        let rows = [facts("current", 3, 50, 0)];
        let mut state = AutomaticSelectionState::default();

        let first = state.evaluate(scope("current"), 1, true, &panel, &rows, idle());
        assert!(matches!(
            first.reason,
            AutoSelectionReason::RouteActivationAwaiting { wins: 1, .. }
        ));
        let duplicate = state.evaluate(scope("current"), 1, true, &panel, &rows, idle());
        assert!(matches!(
            duplicate.reason,
            AutoSelectionReason::DuplicateRound { round_id: 1 }
        ));
        let second = state.evaluate(scope("current"), 2, true, &panel, &rows, idle());
        assert!(second.activate_route);
        assert!(second.target_node.is_none());
        assert!(matches!(
            second.reason,
            AutoSelectionReason::RouteActivationConfirmed { .. }
        ));
    }

    #[test]
    fn traffic_tracker_uses_exact_chain_and_strict_sixty_four_kib_threshold() {
        let started = Instant::now();
        let selection_scope = scope("current");
        let baseline = connections(&[
            ("current", 0, 0, &["current", "select"]),
            ("other", 0, 0, &["current-backup", "select"]),
        ]);
        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(selection_scope.clone(), started, &baseline);
        assert_eq!(
            tracker.status(&selection_scope, started),
            ActiveNodeTransfer::Warming { observed_millis: 0 }
        );

        tracker.observe(
            selection_scope.clone(),
            started + ACTIVE_TRANSFER_WINDOW,
            &connections(&[
                ("current", 0, ACTIVE_TRANSFER_THRESHOLD_BYTES, &["current"]),
                ("other", 0, 1_000_000, &["current-backup"]),
            ]),
        );
        assert_eq!(
            tracker.status(&selection_scope, started + ACTIVE_TRANSFER_WINDOW),
            ActiveNodeTransfer::Idle {
                growth_bytes: ACTIVE_TRANSFER_THRESHOLD_BYTES
            }
        );

        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(selection_scope.clone(), started, &baseline);
        tracker.observe(
            selection_scope.clone(),
            started + ACTIVE_TRANSFER_WINDOW,
            &connections(&[(
                "current",
                0,
                ACTIVE_TRANSFER_THRESHOLD_BYTES + 1,
                &["current"],
            )]),
        );
        assert_eq!(
            tracker.status(&selection_scope, started + ACTIVE_TRANSFER_WINDOW),
            ActiveNodeTransfer::Active {
                growth_bytes: ACTIVE_TRANSFER_THRESHOLD_BYTES + 1
            }
        );
    }

    #[test]
    fn traffic_tracker_counts_new_connections_and_never_subtracts_disappearance() {
        let started = Instant::now();
        let selection_scope = scope("current");
        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(
            selection_scope.clone(),
            started,
            &connections(&[("old", 40, 60, &["current"])]),
        );
        tracker.observe(
            selection_scope.clone(),
            started + Duration::from_secs(5),
            &ConnectionsSnapshot::default(),
        );
        tracker.observe(
            selection_scope.clone(),
            started + ACTIVE_TRANSFER_WINDOW,
            &connections(&[
                ("old", 40, 60, &["current"]),
                ("new", 20_000, 50_000, &["current"]),
            ]),
        );
        assert_eq!(
            tracker.status(&selection_scope, started + ACTIVE_TRANSFER_WINDOW),
            ActiveNodeTransfer::Active {
                growth_bytes: 70_100
            }
        );
    }

    #[test]
    fn traffic_tracker_forgets_disappeared_connection_ids() {
        let started = Instant::now();
        let selection_scope = scope("current");
        let rows = (0..2_000)
            .map(|index| (format!("flow-{index}"), index as u64))
            .collect::<Vec<_>>();
        let first = ConnectionsSnapshot {
            connections: rows
                .iter()
                .map(|(id, total)| ConnectionInfo {
                    id: id.clone(),
                    download: *total,
                    upload: 0,
                    start: None,
                    chains: vec!["current".to_string()],
                    rule: None,
                    rule_payload: None,
                    metadata: ConnectionMetadata::default(),
                })
                .collect(),
            ..ConnectionsSnapshot::default()
        };
        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(selection_scope.clone(), started, &first);
        assert_eq!(tracker.tracked_connection_count(), 2_000);

        tracker.observe(
            selection_scope,
            started + Duration::from_secs(1),
            &ConnectionsSnapshot::default(),
        );
        assert_eq!(tracker.tracked_connection_count(), 0);
    }

    #[test]
    fn controller_failure_discards_window_and_recovery_requires_fresh_ten_seconds() {
        let started = Instant::now();
        let selection_scope = scope("current");
        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(
            selection_scope.clone(),
            started,
            &ConnectionsSnapshot::default(),
        );
        tracker.mark_unavailable("controller timeout");
        assert!(matches!(
            tracker.status(&selection_scope, started + Duration::from_secs(1)),
            ActiveNodeTransfer::Unavailable { .. }
        ));

        let recovered = started + Duration::from_secs(2);
        tracker.observe(
            selection_scope.clone(),
            recovered,
            &ConnectionsSnapshot::default(),
        );
        assert_eq!(
            tracker.status(&selection_scope, recovered + Duration::from_secs(9)),
            ActiveNodeTransfer::Unavailable {
                detail: "connection snapshot is stale".to_string()
            }
        );
        tracker.observe(
            selection_scope.clone(),
            recovered + ACTIVE_TRANSFER_WINDOW,
            &ConnectionsSnapshot::default(),
        );
        assert_eq!(
            tracker.status(&selection_scope, recovered + ACTIVE_TRANSFER_WINDOW),
            ActiveNodeTransfer::Idle { growth_bytes: 0 }
        );
    }

    #[test]
    fn traffic_scope_change_starts_a_new_baseline() {
        let started = Instant::now();
        let old_scope = scope("current");
        let mut new_scope = scope("replacement");
        new_scope.panel = NodeViewId::streaming();
        let mut tracker = ActiveNodeTrafficTracker::default();
        tracker.observe(
            old_scope,
            started,
            &connections(&[("one", 0, 90_000, &["current"])]),
        );
        tracker.observe(
            new_scope.clone(),
            started + ACTIVE_TRANSFER_WINDOW,
            &connections(&[("two", 0, 90_000, &["replacement"])]),
        );
        assert_eq!(
            tracker.status(&new_scope, started + ACTIVE_TRANSFER_WINDOW),
            ActiveNodeTransfer::Warming { observed_millis: 0 }
        );
    }
}
