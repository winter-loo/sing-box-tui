use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::time::Duration;

use reqwest::Client as AsyncClient;
use serde::{Deserialize, Serialize};
use tokio::runtime::Builder as TokioRuntimeBuilder;

use crate::land_mask_data::{LAND_MASK, LAND_MASK_HEIGHT, LAND_MASK_WIDTH};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NodeTone {
    Success,
    Error,
    Pending,
    Missing,
}

/// Checks whether a geographic coordinate (lon, lat) falls on land
/// using the pre-rasterized 160x60 Natural Earth landmask.
#[allow(dead_code)]
pub fn is_land(lon: f64, lat: f64) -> bool {
    let u = ((lon + 180.0) / 360.0).clamp(0.0, 1.0);
    let v = ((90.0 - lat) / 180.0).clamp(0.0, 1.0);
    let mx = ((u * LAND_MASK_WIDTH as f64) as usize).min(LAND_MASK_WIDTH - 1);
    let my = ((v * LAND_MASK_HEIGHT as f64) as usize).min(LAND_MASK_HEIGHT - 1);
    let idx = my * LAND_MASK_WIDTH + mx;
    let byte = LAND_MASK[idx / 8];
    (byte & (1 << (7 - (idx % 8)))) != 0
}

/// Checks whether a normalized grid position (x in 0..width, y in 0..height) is land.
pub fn is_land_grid(x: usize, y: usize, width: usize, height: usize) -> bool {
    if width == 0 || height == 0 {
        return false;
    }
    let mx = ((x as f64 / width as f64) * LAND_MASK_WIDTH as f64) as usize;
    let my = ((y as f64 / height as f64) * LAND_MASK_HEIGHT as f64) as usize;
    let mx = mx.min(LAND_MASK_WIDTH - 1);
    let my = my.min(LAND_MASK_HEIGHT - 1);
    let idx = my * LAND_MASK_WIDTH + mx;
    let byte = LAND_MASK[idx / 8];
    (byte & (1 << (7 - (idx % 8)))) != 0
}

/// Projects (lat, lon) onto a 2D terminal canvas of size (width, height).
/// Returns (x, y) where 0 <= x < width and 0 <= y < height.
pub fn project_coords(lat: f64, lon: f64, width: usize, height: usize) -> (usize, usize) {
    if width == 0 || height == 0 {
        return (0, 0);
    }
    let x_norm = ((lon + 180.0) / 360.0).clamp(0.0, 1.0);
    let y_norm = ((90.0 - lat) / 180.0).clamp(0.0, 1.0);

    let x = (x_norm * (width.saturating_sub(1)) as f64).round() as usize;
    let y = (y_norm * (height.saturating_sub(1)) as f64).round() as usize;
    (x.min(width.saturating_sub(1)), y.min(height.saturating_sub(1)))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GeoLocation {
    pub country: String,
    pub country_code: String,
    pub region: String,
    pub city: String,
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeLocation {
    pub tag: String,
    pub outbound_type: String,
    pub server_host: String,
    pub server_port: Option<u16>,
    pub ip: Option<String>,
    pub location: Option<GeoLocation>,
    pub is_current: bool,
    pub latency_ms: Option<u64>,
    pub reachability: String,
    pub tone: NodeTone,
}

impl NodeLocation {
    pub fn is_reachable(&self) -> bool {
        self.tone == NodeTone::Success
    }

    pub fn display_location(&self) -> String {
        match &self.location {
            Some(loc) => {
                let flag = country_code_to_flag(&loc.country_code);
                let city_part = if !loc.city.is_empty() {
                    format!("{}, ", loc.city)
                } else if !loc.region.is_empty() {
                    format!("{}, ", loc.region)
                } else {
                    String::new()
                };
                if !flag.is_empty() {
                    format!("{flag} {city_part}{}", loc.country)
                } else {
                    format!("{city_part}{}", loc.country)
                }
            }
            None => "Unknown Location".to_string(),
        }
    }

    pub fn display_coordinates(&self) -> String {
        match &self.location {
            Some(loc) => {
                let lat_dir = if loc.latitude >= 0.0 { 'N' } else { 'S' };
                let lon_dir = if loc.longitude >= 0.0 { 'E' } else { 'W' };
                format!(
                    "{:.2}° {}, {:.2}° {}",
                    loc.latitude.abs(),
                    lat_dir,
                    loc.longitude.abs(),
                    lon_dir
                )
            }
            None => "Disconnected / Unresolved".to_string(),
        }
    }
}

pub fn country_code_to_flag(code: &str) -> String {
    let code = code.trim().to_uppercase();
    if code.len() != 2 {
        return String::new();
    }
    let chars: Vec<char> = code.chars().collect();
    if chars[0].is_ascii_uppercase() && chars[1].is_ascii_uppercase() {
        let base: u32 = 0x1F1E6 - 0x41;
        let c1 = char::from_u32(chars[0] as u32 + base).unwrap_or(' ');
        let c2 = char::from_u32(chars[1] as u32 + base).unwrap_or(' ');
        format!("{c1}{c2}")
    } else {
        String::new()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct NodeMapStats {
    pub total_nodes: usize,
    pub plotted_nodes: usize,
    pub reachable_nodes: usize,
    pub unique_countries: usize,
}

pub struct NodeMapState {
    pub nodes: Vec<NodeLocation>,
    pub selected_index: usize,
    pub filter_reachable_only: bool,
    pub is_resolving: bool,
}

impl NodeMapState {
    pub fn new(nodes: Vec<NodeLocation>) -> Self {
        let mut state = Self {
            nodes,
            selected_index: 0,
            filter_reachable_only: false,
            is_resolving: false,
        };
        // Auto-select current active node if available
        if let Some(pos) = state.nodes.iter().position(|n| n.is_current) {
            state.selected_index = pos;
        }
        state
    }

    pub fn filtered_indices(&self) -> Vec<usize> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| !self.filter_reachable_only || node.is_reachable())
            .map(|(i, _)| i)
            .collect()
    }

    pub fn selected_node(&self) -> Option<&NodeLocation> {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return self.nodes.get(self.selected_index);
        }
        let target = self.selected_index.min(filtered.len().saturating_sub(1));
        filtered.get(target).and_then(|&idx| self.nodes.get(idx))
    }

    pub fn select_next(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        if self.selected_index + 1 < filtered.len() {
            self.selected_index += 1;
        } else {
            self.selected_index = 0;
        }
    }

    pub fn select_previous(&mut self) {
        let filtered = self.filtered_indices();
        if filtered.is_empty() {
            return;
        }
        if self.selected_index > 0 {
            self.selected_index -= 1;
        } else {
            self.selected_index = filtered.len().saturating_sub(1);
        }
    }

    pub fn select_first(&mut self) {
        self.selected_index = 0;
    }

    pub fn select_last(&mut self) {
        let filtered = self.filtered_indices();
        self.selected_index = filtered.len().saturating_sub(1);
    }

    pub fn toggle_filter(&mut self) {
        self.filter_reachable_only = !self.filter_reachable_only;
        self.selected_index = 0;
    }

    pub fn stats(&self) -> NodeMapStats {
        let total = self.nodes.len();
        let plotted = self.nodes.iter().filter(|n| n.location.is_some()).count();
        let reachable = self.nodes.iter().filter(|n| n.is_reachable()).count();
        let countries: BTreeSet<&str> = self
            .nodes
            .iter()
            .filter_map(|n| n.location.as_ref().map(|l| l.country_code.as_str()))
            .collect();
        NodeMapStats {
            total_nodes: total,
            plotted_nodes: plotted,
            reachable_nodes: reachable,
            unique_countries: countries.len(),
        }
    }

    pub fn apply_resolved_locations(&mut self, updates: &[(String, Option<String>, Option<GeoLocation>)]) {
        for (tag, ip, loc) in updates {
            if let Some(node) = self.nodes.iter_mut().find(|n| &n.tag == tag) {
                if let Some(resolved_ip) = ip {
                    node.ip = Some(resolved_ip.clone());
                }
                if let Some(geo) = loc {
                    node.location = Some(geo.clone());
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct IpApiBatchItem {
    status: String,
    country: Option<String>,
    #[serde(rename = "countryCode")]
    country_code: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    city: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    query: Option<String>,
}

/// Resolves IPs and geolocations asynchronously in a background worker thread.
/// Takes server hosts, resolves DNS, queries `ip-api.com/batch` (via sing-box local proxy or direct),
/// and invokes callback with results.
pub fn spawn_ip_geolocation_worker<F>(
    targets: Vec<(String, String)>, // (node_tag, server_host)
    local_proxy_port: Option<u16>,
    on_complete: F,
) where
    F: FnOnce(Vec<(String, Option<String>, Option<GeoLocation>)>) + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = match TokioRuntimeBuilder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return,
        };

        let results = runtime.block_on(async move {
            let mut resolved: Vec<(String, Option<String>, Option<GeoLocation>)> = Vec::new();
            let mut ip_to_tags: BTreeMap<String, Vec<String>> = BTreeMap::new();

            // 1. Resolve hostnames to IP
            for (tag, host) in targets {
                if host.is_empty() {
                    resolved.push((tag, None, None));
                    continue;
                }
                let ip_str = if let Ok(ip) = host.parse::<IpAddr>() {
                    Some(ip.to_string())
                } else {
                    let lookup = tokio::time::timeout(
                        Duration::from_millis(1500),
                        tokio::net::lookup_host(format!("{host}:443")),
                    )
                    .await;
                    match lookup {
                        Ok(Ok(mut addrs)) => addrs.next().map(|a| a.ip().to_string()),
                        _ => None,
                    }
                };

                if let Some(ip) = ip_str {
                    ip_to_tags.entry(ip.clone()).or_default().push(tag.clone());
                    resolved.push((tag, Some(ip), None));
                } else {
                    resolved.push((tag, None, None));
                }
            }

            // 2. Query ip-api.com/batch for unique IPs
            let unique_ips: Vec<String> = ip_to_tags.keys().cloned().collect();
            if !unique_ips.is_empty() {
                // Build client with local proxy if available
                let mut client_builder = AsyncClient::builder().timeout(Duration::from_secs(3));
                if let Some(port) = local_proxy_port {
                    if let Ok(proxy) = reqwest::Proxy::all(format!("http://127.0.0.1:{port}")) {
                        client_builder = client_builder.proxy(proxy);
                    }
                }
                let client = client_builder.build().unwrap_or_default();

                for chunk in unique_ips.chunks(100) {
                    let request_body = serde_json::to_string(&chunk).unwrap_or_default();
                    let response = client
                        .post("http://ip-api.com/batch")
                        .header("Content-Type", "application/json")
                        .body(request_body)
                        .send()
                        .await;

                    if let Ok(resp) = response {
                        if let Ok(items) = resp.json::<Vec<IpApiBatchItem>>().await {
                            for item in items {
                                if item.status == "success" {
                                    if let (Some(query_ip), Some(lat), Some(lon)) =
                                        (item.query, item.lat, item.lon)
                                    {
                                        let geo = GeoLocation {
                                            country: item.country.unwrap_or_default(),
                                            country_code: item.country_code.unwrap_or_default(),
                                            region: item.region_name.unwrap_or_default(),
                                            city: item.city.unwrap_or_default(),
                                            latitude: lat,
                                            longitude: lon,
                                        };
                                        for (_tag, ip, loc) in resolved.iter_mut() {
                                            if ip.as_deref() == Some(&query_ip) {
                                                *loc = Some(geo.clone());
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            resolved
        });

        on_complete(results);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn land_mask_identifies_major_continents() {
        // Beijing (Asia): 116.4 E, 39.9 N
        assert!(is_land(116.4, 39.9));
        // North America: -98.0 W, 38.0 N
        assert!(is_land(-98.0, 38.0));
        // Australia: 135.0 E, -25.0 S
        assert!(is_land(135.0, -25.0));
        // Pacific Ocean: -160.0 W, 0.0 N
        assert!(!is_land(-160.0, 0.0));
    }

    #[test]
    fn coordinate_projection_stays_in_bounds() {
        let (x, y) = project_coords(35.68, 139.65, 80, 24);
        assert!(x < 80);
        assert!(y < 24);

        let (x_min, y_min) = project_coords(90.0, -180.0, 80, 24);
        assert_eq!(x_min, 0);
        assert_eq!(y_min, 0);

        let (x_max, y_max) = project_coords(-90.0, 180.0, 80, 24);
        assert_eq!(x_max, 79);
        assert_eq!(y_max, 23);
    }

    #[test]
    fn node_map_state_navigation() {
        let nodes = vec![
            NodeLocation {
                tag: "node-1".to_string(),
                outbound_type: "vless".to_string(),
                server_host: "us.example.com".to_string(),
                server_port: Some(443),
                ip: Some("1.2.3.4".to_string()),
                location: Some(GeoLocation {
                    country: "United States".to_string(),
                    country_code: "US".to_string(),
                    region: "CA".to_string(),
                    city: "Los Angeles".to_string(),
                    latitude: 34.05,
                    longitude: -118.24,
                }),
                is_current: true,
                latency_ms: Some(120),
                reachability: "stable reachable".to_string(),
                tone: NodeTone::Success,
            },
            NodeLocation {
                tag: "node-2".to_string(),
                outbound_type: "vmess".to_string(),
                server_host: "jp.example.com".to_string(),
                server_port: Some(443),
                ip: None,
                location: None,
                is_current: false,
                latency_ms: None,
                reachability: "unreachable".to_string(),
                tone: NodeTone::Error,
            },
        ];

        let mut state = NodeMapState::new(nodes);
        assert_eq!(state.selected_index, 0);
        state.select_next();
        assert_eq!(state.selected_index, 1);
        state.select_next();
        assert_eq!(state.selected_index, 0);

        state.toggle_filter();
        assert!(state.filter_reachable_only);
        assert_eq!(state.filtered_indices().len(), 1);
    }
}
