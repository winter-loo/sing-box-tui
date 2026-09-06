use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use anyhow::Result;
use serde_json::Value;

use super::App;
use crate::controller::ProbeOutcome;
use crate::node_map::{
    GeoLocation, NodeLocation, NodeMapState, NodeTone, infer_location_from_name,
    spawn_ip_geolocation_worker,
};

pub(super) struct NodeMapJob {
    receiver: Receiver<Vec<(String, Option<String>, Option<GeoLocation>)>>,
}

impl App {
    pub(super) fn open_node_map(&mut self) -> Result<()> {
        let config_content =
            std::fs::read_to_string(&self.system_proxy_config_path).unwrap_or_default();
        let config_json: Value = serde_json::from_str(&config_content).unwrap_or_default();

        let mut outbounds_map: BTreeMap<String, (String, String, Option<u16>)> = BTreeMap::new();
        if let Some(outbounds) = config_json.get("outbounds").and_then(Value::as_array) {
            for ob in outbounds {
                if let Some(tag) = ob.get("tag").and_then(Value::as_str) {
                    let ob_type = ob
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("proxy")
                        .to_string();
                    let server = ob
                        .get("server")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let server_port = ob.get("server_port").and_then(Value::as_u64).map(|p| p as u16);
                    outbounds_map.insert(tag.to_string(), (ob_type, server, server_port));
                }
            }
        }

        let selected_group = self.groups.get(self.group_index);
        let group_name = selected_group.map(|g| g.name.as_str()).unwrap_or("");
        let current_member = selected_group.and_then(|g| g.current.clone());

        // Get members from current selector
        let members: Vec<String> = if let Some(group) = selected_group {
            group.members.clone()
        } else {
            outbounds_map.keys().cloned().collect()
        };

        let mut node_locations = Vec::new();
        let mut targets_for_resolution = Vec::new();

        for member in &members {
            let (ob_type, server, port) = outbounds_map
                .get(member)
                .cloned()
                .unwrap_or_else(|| ("proxy".to_string(), String::new(), None));

            let stored_assessment =
                self.benchmark_workflow.reachability_assessment(group_name, member);
            let tone = if self
                .benchmark_workflow
                .active_quick_probe(group_name, member)
                .is_some()
            {
                NodeTone::Pending
            } else {
                match stored_assessment.and_then(|a| a.assessment) {
                    Some(crate::controller::ReachabilityAssessment::StableReachable)
                    | Some(crate::controller::ReachabilityAssessment::Reachable) => NodeTone::Success,
                    Some(crate::controller::ReachabilityAssessment::Degraded)
                    | Some(crate::controller::ReachabilityAssessment::Unreachable) => NodeTone::Error,
                    None => NodeTone::Missing,
                }
            };

            let reachability_label = stored_assessment
                .and_then(|a| a.assessment)
                .map(|a| a.label().to_string())
                .unwrap_or_else(|| "--".to_string());

            let latency_ms = stored_assessment.and_then(|a| {
                let reachable: Vec<u64> = a
                    .attempts
                    .iter()
                    .filter_map(|att| match att {
                        ProbeOutcome::Reachable { delay_ms } => Some(*delay_ms),
                        _ => None,
                    })
                    .collect();
                if reachable.is_empty() {
                    None
                } else {
                    Some(reachable.iter().sum::<u64>() / reachable.len() as u64)
                }
            });

            let is_current = current_member.as_deref() == Some(member.as_str());
            let initial_location = infer_location_from_name(member);

            node_locations.push(NodeLocation {
                tag: member.clone(),
                outbound_type: ob_type,
                server_host: server.clone(),
                server_port: port,
                ip: None,
                location: initial_location,
                is_current,
                latency_ms,
                reachability: reachability_label,
                tone,
            });

            if !server.is_empty() {
                targets_for_resolution.push((member.clone(), server));
            }
        }

        let mut state = NodeMapState::new(node_locations);
        state.is_resolving = !targets_for_resolution.is_empty();
        self.node_map = Some(state);

        if !targets_for_resolution.is_empty() {
            let (tx, rx) = mpsc::channel();
            // Local proxy port: check inbounds or default 6780
            let proxy_port = Some(6780);
            spawn_ip_geolocation_worker(targets_for_resolution, proxy_port, move |results| {
                let _ = tx.send(results);
            });
            self.node_map_job = Some(NodeMapJob { receiver: rx });
        }

        self.flash = None;
        self.set_status_only("Opened node location world map (Esc/Enter/M to close)");
        Ok(())
    }

    pub(super) fn close_node_map(&mut self) {
        self.node_map = None;
        self.node_map_job = None;
        self.set_status_only("Node location world map closed");
    }

    pub(super) fn poll_node_map_updates(&mut self) {
        if let Some(job) = &self.node_map_job {
            match job.receiver.try_recv() {
                Ok(updates) => {
                    if let Some(state) = &mut self.node_map {
                        state.apply_resolved_locations(&updates);
                        state.is_resolving = false;
                        self.set_status_only("Node IP physical locations updated");
                    }
                    self.node_map_job = None;
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    if let Some(state) = &mut self.node_map {
                        state.is_resolving = false;
                    }
                    self.node_map_job = None;
                }
            }
        }
    }

    pub(super) fn refresh_node_map_geolocation(&mut self) {
        if let Some(state) = &mut self.node_map {
            if state.is_resolving {
                return;
            }
            let targets: Vec<(String, String)> = state
                .nodes
                .iter()
                .filter(|n| !n.server_host.is_empty())
                .map(|n| (n.tag.clone(), n.server_host.clone()))
                .collect();
            if !targets.is_empty() {
                state.is_resolving = true;
                let (tx, rx) = mpsc::channel();
                let proxy_port = Some(6780);
                spawn_ip_geolocation_worker(targets, proxy_port, move |results| {
                    let _ = tx.send(results);
                });
                self.node_map_job = Some(NodeMapJob { receiver: rx });
                self.set_status_only("Refreshing node IP physical locations...");
            }
        }
    }
}
