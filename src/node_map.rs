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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LocalEgressInfo {
    pub ip: String,
    pub isp: String,
    pub location: String,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

const CN_CITY_COORDS: &[(&[&str], f64, f64)] = &[
    (&["杭州", "HANGZHOU"], 30.2741, 120.1551),
    (&["上海", "SHANGHAI"], 31.2304, 121.4737),
    (&["北京", "BEIJING"], 39.9042, 116.4074),
    (&["广州", "GUANGZHOU"], 23.1291, 113.2644),
    (&["深圳", "SHENZHEN"], 22.5431, 114.0579),
    (&["成都", "CHENGDU"], 30.5728, 104.0668),
    (&["武汉", "WUHAN"], 30.5928, 114.3055),
    (&["南京", "NANJING"], 32.0603, 118.7969),
    (&["西安", "XIAN", "XI'AN"], 34.3416, 108.9398),
    (&["重庆", "CHONGQING"], 29.5630, 106.5516),
    (&["天津", "TIANJIN"], 39.1256, 117.1902),
    (&["苏州", "SUZHOU"], 31.2990, 120.5853),
    (&["郑州", "ZHENGZHOU"], 34.7466, 113.6253),
    (&["长沙", "CHANGSHA"], 28.2282, 112.9388),
    (&["沈阳", "SHENYANG"], 41.8057, 123.4315),
    (&["青岛", "QINGDAO"], 36.0671, 120.3826),
    (&["宁波", "NINGBO"], 29.8683, 121.5440),
    (&["温州", "WENZHOU"], 28.0006, 120.6994),
    (&["福州", "FUZHOU"], 26.0745, 119.2965),
    (&["厦门", "XIAMEN"], 24.4798, 118.0894),
    (&["济南", "JINAN"], 36.6512, 117.1201),
    (&["合肥", "HEFEI"], 31.8206, 117.2272),
    (&["昆明", "KUNMING"], 25.0406, 102.7123),
    (&["哈尔滨", "HARBIN"], 45.8038, 126.5350),
    (&["大连", "DALIAN"], 38.9140, 121.6147),
    (&["长春", "CHANGCHUN"], 43.8171, 125.3235),
    (&["南宁", "NANNING"], 22.8170, 108.3665),
    (&["贵阳", "GUIYANG"], 26.6470, 106.6302),
    (&["太原", "TAIYUAN"], 37.8706, 112.5489),
    (&["乌鲁木齐", "URUMQI"], 43.8256, 87.6168),
    (&["海口", "HAIKOU"], 20.0440, 110.1999),
    (&["兰州", "LANZHOU"], 36.0611, 103.8343),
    (&["银川", "YINCHUAN"], 38.4872, 106.2309),
    (&["西宁", "XINING"], 36.6232, 101.7789),
    (&["呼和浩特", "HOHHOT"], 40.8415, 111.7519),
    (&["拉萨", "LHASA"], 29.6525, 91.1721),
    (&["浙江", "ZHEJIANG"], 30.2674, 120.1528),
    (&["江苏", "JIANGSU"], 32.0617, 118.7632),
    (&["广东", "GUANGDONG"], 23.1322, 113.2665),
    (&["山东", "SHANDONG"], 36.6686, 117.0208),
    (&["河南", "HENAN"], 34.7657, 113.6539),
    (&["四川", "SICHUAN"], 30.6517, 104.0759),
    (&["湖北", "HUBEI"], 30.5454, 114.3423),
    (&["湖南", "HUNAN"], 28.1128, 112.9838),
    (&["河北", "HEBEI"], 38.0428, 114.5149),
    (&["福建", "FUJIAN"], 26.0789, 119.3062),
];

pub fn find_city_coords(location_str: &str) -> Option<(f64, f64)> {
    let upper = location_str.to_uppercase();
    for (keywords, lat, lon) in CN_CITY_COORDS {
        for kw in *keywords {
            if upper.contains(kw) {
                return Some((*lat, *lon));
            }
        }
    }
    None
}

pub fn normalize_isp(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.contains("电信") {
        "中国电信 (China Telecom)".to_string()
    } else if trimmed.contains("联通") {
        "中国联通 (China Unicom)".to_string()
    } else if trimmed.contains("移动") {
        "中国移动 (China Mobile)".to_string()
    } else if trimmed.contains("广电") {
        "中国广电 (China Broadnet)".to_string()
    } else if trimmed.contains("铁通") {
        "中国铁通 (China Tietong)".to_string()
    } else if trimmed.contains("教育") {
        "中国教育网 (CERNET)".to_string()
    } else if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        "未知运营商".to_string()
    }
}

pub fn parse_myip_ipip_net(text: &str) -> Option<LocalEgressInfo> {
    // Expected: "当前 IP：220.184.215.38  来自于：中国 浙江 杭州  电信"
    let ip_idx = text.find("当前 IP：")?;
    let from_idx = text.find("来自于：")?;
    if from_idx <= ip_idx {
        return None;
    }
    let ip = text[ip_idx + "当前 IP：".len()..from_idx].trim().to_string();
    let after_from = text[from_idx + "来自于：".len()..].trim();
    let tokens: Vec<&str> = after_from.split_whitespace().collect();
    if tokens.is_empty() {
        return None;
    }

    let (location, raw_isp) = if tokens.len() >= 2 {
        let last = tokens.last().copied().unwrap_or_default();
        if last.contains("电信")
            || last.contains("联通")
            || last.contains("移动")
            || last.contains("广电")
            || last.contains("铁通")
            || last.contains("教育")
        {
            (tokens[..tokens.len() - 1].join(" "), last)
        } else {
            (tokens.join(" "), "")
        }
    } else {
        (tokens.join(" "), "")
    };

    let isp = normalize_isp(raw_isp);
    let (latitude, longitude) = find_city_coords(&location)
        .map(|(lat, lon)| (Some(lat), Some(lon)))
        .unwrap_or((None, None));

    Some(LocalEgressInfo {
        ip,
        isp,
        location,
        latitude,
        longitude,
    })
}

pub fn parse_cip_cc(text: &str) -> Option<LocalEgressInfo> {
    let mut ip = String::new();
    let mut location = String::new();
    let mut raw_isp = String::new();

    for line in text.lines() {
        let line = line.trim();
        if let Some(val) = line.strip_prefix("IP\t:") {
            ip = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("IP :") {
            ip = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("地址\t:") {
            location = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("地址 :") {
            location = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("运营商\t:") {
            raw_isp = val.trim().to_string();
        } else if let Some(val) = line.strip_prefix("运营商 :") {
            raw_isp = val.trim().to_string();
        }
    }

    if ip.is_empty() {
        return None;
    }

    let isp = normalize_isp(&raw_isp);
    let (latitude, longitude) = find_city_coords(&location)
        .map(|(lat, lon)| (Some(lat), Some(lon)))
        .unwrap_or((None, None));

    Some(LocalEgressInfo {
        ip,
        isp,
        location,
        latitude,
        longitude,
    })
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
    pub local_egress: Option<LocalEgressInfo>,
}

impl NodeMapState {
    pub fn new(nodes: Vec<NodeLocation>) -> Self {
        let mut state = Self {
            nodes,
            selected_index: 0,
            filter_reachable_only: false,
            is_resolving: false,
            local_egress: None,
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

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NodeMapWorkerResult {
    pub node_locations: Vec<(String, Option<String>, Option<GeoLocation>)>,
    pub local_egress: Option<LocalEgressInfo>,
}

pub async fn fetch_local_egress() -> Option<LocalEgressInfo> {
    let client = AsyncClient::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;

    // 1. Try myip.ipip.net (domestic CN, fast text response)
    if let Ok(resp) = client.get("http://myip.ipip.net").send().await {
        if let Ok(text) = resp.text().await {
            if let Some(info) = parse_myip_ipip_net(&text) {
                return Some(info);
            }
        }
    }

    // 2. Try cip.cc (domestic CN fallback)
    if let Ok(resp) = client
        .get("http://cip.cc")
        .header("User-Agent", "curl/7.88.1")
        .send()
        .await
    {
        if let Ok(text) = resp.text().await {
            if let Some(info) = parse_cip_cc(&text) {
                return Some(info);
            }
        }
    }

    // 3. Fallback: ip-api.com
    if let Ok(resp) = client
        .get("http://ip-api.com/json/?fields=status,country,city,lat,lon,isp,query")
        .send()
        .await
    {
        if let Ok(val) = resp.json::<serde_json::Value>().await {
            if val.get("status").and_then(|s| s.as_str()) == Some("success") {
                let ip = val
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let isp_raw = val.get("isp").and_then(|v| v.as_str()).unwrap_or_default();
                let country = val.get("country").and_then(|v| v.as_str()).unwrap_or_default();
                let city = val.get("city").and_then(|v| v.as_str()).unwrap_or_default();
                let lat = val.get("lat").and_then(|v| v.as_f64());
                let lon = val.get("lon").and_then(|v| v.as_f64());
                let loc = if !city.is_empty() {
                    format!("{country} {city}")
                } else {
                    country.to_string()
                };
                return Some(LocalEgressInfo {
                    ip,
                    isp: normalize_isp(isp_raw),
                    location: loc,
                    latitude: lat,
                    longitude: lon,
                });
            }
        }
    }

    None
}

/// Resolves IPs and geolocations asynchronously in a background worker thread.
/// Concurrently resolves node server hostnames/IPs and probes local exit broadband info.
pub fn spawn_ip_geolocation_worker<F>(
    targets: Vec<(String, String)>, // (node_tag, server_host)
    local_proxy_port: Option<u16>,
    on_complete: F,
) where
    F: FnOnce(NodeMapWorkerResult) + Send + 'static,
{
    std::thread::spawn(move || {
        let runtime = match TokioRuntimeBuilder::new_current_thread().enable_all().build() {
            Ok(rt) => rt,
            Err(_) => return,
        };

        let results = runtime.block_on(async move {
            let local_egress_future = fetch_local_egress();
            let nodes_future = async {
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
            };

            let (local_egress, node_locations) = tokio::join!(local_egress_future, nodes_future);
            NodeMapWorkerResult {
                node_locations,
                local_egress,
            }
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

    #[test]
    fn parse_myip_ipip_net_identifies_telecom_and_hangzhou() {
        let sample = "当前 IP：220.184.215.38  来自于：中国 浙江 杭州  电信";
        let info = parse_myip_ipip_net(sample).expect("parsed");
        assert_eq!(info.ip, "220.184.215.38");
        assert!(info.isp.contains("中国电信"));
        assert_eq!(info.location, "中国 浙江 杭州");
        assert!(info.latitude.is_some());
        assert!(info.longitude.is_some());
    }

    #[test]
    fn parse_cip_cc_identifies_unicom() {
        let sample = "IP\t: 114.240.12.34\n地址\t: 中国 北京\n运营商\t: 联通\n";
        let info = parse_cip_cc(sample).expect("parsed");
        assert_eq!(info.ip, "114.240.12.34");
        assert!(info.isp.contains("中国联通"));
    }
}
