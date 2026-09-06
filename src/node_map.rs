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
            None => "--, --".to_string(),
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

/// Fast offline heuristic mapping for node tags when offline or waiting for API resolution.
pub fn infer_location_from_name(name: &str) -> Option<GeoLocation> {
    let upper = name.to_uppercase();

    // Map common regions: (keywords, country, code, region, city, lat, lon)
    const PRESETS: &[(&[&str], &str, &str, &str, &str, f64, f64)] = &[
        (&["香港", "HK", "HONG KONG", "HONGKONG", "🇭🇰"], "Hong Kong", "HK", "Hong Kong", "Hong Kong", 22.3193, 114.1694),
        (&["台湾", "臺灣", "TW", "TAIWAN", "TAIPEI", "台北", "臺北", "🇹🇼"], "Taiwan", "TW", "Taipei", "Taipei", 25.0330, 121.5654),
        (&["日本", "JP", "JAPAN", "TOKYO", "东京", "東京", "OSAKA", "大阪", "🇯🇵"], "Japan", "JP", "Kanto", "Tokyo", 35.6762, 139.6503),
        (&["新加坡", "SG", "SINGAPORE", "LION", "狮城", "🇸🇬"], "Singapore", "SG", "Singapore", "Singapore", 1.3521, 103.8198),
        (&["美国", "美國", "US", "USA", "UNITED STATES", "AMERICA", "LOS ANGELES", "洛杉矶", "SAN JOSE", "圣何塞", "SILICON", "硅谷", "NEW YORK", "纽约", "SEATTLE", "西雅图", "CHICAGO", "芝加哥", "🇺🇸"], "United States", "US", "California", "Los Angeles", 34.0522, -118.2437),
        (&["韩国", "韓國", "KR", "KOREA", "SEOUL", "首尔", "首爾", "🇰🇷"], "South Korea", "KR", "Seoul", "Seoul", 37.5665, 126.9780),
        (&["英国", "英國", "UK", "GB", "UNITED KINGDOM", "BRITAIN", "LONDON", "伦敦", "倫敦", "🇬🇧"], "United Kingdom", "GB", "England", "London", 51.5074, -0.1278),
        (&["德国", "德國", "DE", "GERMANY", "FRANKFURT", "法兰克福", "BERLIN", "柏林", "🇩🇪"], "Germany", "DE", "Hesse", "Frankfurt", 50.1109, 8.6821),
        (&["法国", "法國", "FR", "FRANCE", "PARIS", "巴黎", "🇫🇷"], "France", "FR", "Ile-de-France", "Paris", 48.8566, 2.3522),
        (&["荷兰", "荷蘭", "NL", "NETHERLANDS", "AMSTERDAM", "阿姆斯特丹", "🇳🇱"], "Netherlands", "NL", "North Holland", "Amsterdam", 52.3676, 4.9041),
        (&["澳大利亚", "澳洲", "AU", "AUSTRALIA", "SYDNEY", "悉尼", "MELBOURNE", "墨尔本", "🇦🇺"], "Australia", "AU", "New South Wales", "Sydney", -33.8688, 151.2093),
        (&["加拿大", "CA", "CANADA", "TORONTO", "多伦多", "VANCOUVER", "温哥华", "🇨🇦"], "Canada", "CA", "Ontario", "Toronto", 43.6532, -79.3832),
        (&["俄罗斯", "俄羅斯", "RU", "RUSSIA", "MOSCOW", "莫斯科", "🇷🇺"], "Russia", "RU", "Moscow", "Moscow", 55.7558, 37.6173),
        (&["印度", "IN", "INDIA", "MUMBAI", "孟买", "DELHI", "德里", "🇮🇳"], "India", "IN", "Maharashtra", "Mumbai", 19.0760, 72.8777),
        (&["马来西亚", "MY", "MALAYSIA", "KUALA LUMPUR", "吉隆坡", "🇲🇾"], "Malaysia", "MY", "Federal Territory", "Kuala Lumpur", 3.1390, 101.6869),
        (&["泰国", "TH", "THAILAND", "BANGKOK", "曼谷", "🇹🇭"], "Thailand", "TH", "Bangkok", "Bangkok", 13.7563, 100.5018),
        (&["越南", "VN", "VIETNAM", "HANOI", "河内", "HO CHI MINH", "胡志明", "🇻🇳"], "Vietnam", "VN", "Hanoi", "Hanoi", 21.0285, 105.8542),
        (&["菲律宾", "PH", "PHILIPPINES", "MANILA", "马尼拉", "🇵🇭"], "Philippines", "PH", "Metro Manila", "Manila", 14.5995, 120.9842),
        (&["印尼", "ID", "INDONESIA", "JAKARTA", "雅加达", "🇮🇩"], "Indonesia", "ID", "Jakarta", "Jakarta", -6.2088, 106.8456),
        (&["土耳其", "TR", "TURKEY", "ISTANBUL", "伊斯坦布尔", "🇹🇷"], "Turkey", "TR", "Istanbul", "Istanbul", 41.0082, 28.9784),
        (&["阿联酋", "迪拜", "AE", "UAE", "DUBAI", "🇦🇪"], "United Arab Emirates", "AE", "Dubai", "Dubai", 25.2048, 55.2708),
        (&["巴西", "BR", "BRAZIL", "SAO PAULO", "圣保罗", "🇧🇷"], "Brazil", "BR", "Sao Paulo", "Sao Paulo", -23.5505, -46.6333),
        (&["阿根廷", "AR", "ARGENTINA", "BUENOS AIRES", "布宜诺斯艾利斯", "🇦🇷"], "Argentina", "AR", "Buenos Aires", "Buenos Aires", -34.6037, -58.3816),
        (&["南非", "ZA", "SOUTH AFRICA", "JOHANNESBURG", "约翰内斯堡", "🇿🇦"], "South Africa", "ZA", "Gauteng", "Johannesburg", -26.2041, 28.0473),
        (&["瑞士", "CH", "SWITZERLAND", "ZURICH", "苏黎世", "🇨🇭"], "Switzerland", "CH", "Zurich", "Zurich", 47.3769, 8.5417),
        (&["瑞典", "SE", "SWEDEN", "STOCKHOLM", "斯德哥尔摩", "🇸🇪"], "Sweden", "SE", "Stockholm", "Stockholm", 59.3293, 18.0686),
        (&["意大利", "IT", "ITALY", "ROME", "罗马", "MILAN", "米兰", "🇮🇹"], "Italy", "IT", "Lazio", "Rome", 41.9028, 12.4964),
        (&["西班牙", "ES", "SPAIN", "MADRID", "马德里", "BARCELONA", "巴塞罗那", "🇪🇸"], "Spain", "ES", "Madrid", "Madrid", 40.4168, -3.7038),
        (&["波兰", "PL", "POLAND", "WARSAW", "华沙", "🇵🇱"], "Poland", "PL", "Mazovia", "Warsaw", 52.2297, 21.0122),
        (&["爱尔兰", "IE", "IRELAND", "DUBLIN", "都柏林", "🇮🇪"], "Ireland", "IE", "Leinster", "Dublin", 53.3498, -6.2603),
        (&["芬兰", "FI", "FINLAND", "HELSINKI", "赫尔辛基", "🇫🇮"], "Finland", "FI", "Uusimaa", "Helsinki", 60.1699, 24.9384),
        (&["挪威", "NO", "NORWAY", "OSLO", "奥斯陆", "🇳🇴"], "Norway", "NO", "Oslo", "Oslo", 59.9139, 10.7522),
        (&["丹麦", "DK", "DENMARK", "COPENHAGEN", "哥本哈根", "🇩🇰"], "Denmark", "DK", "Capital Region", "Copenhagen", 55.6761, 12.5683),
        (&["奥地利", "AT", "AUSTRIA", "VIENNA", "维也纳", "🇦🇹"], "Austria", "AT", "Vienna", "Vienna", 48.2082, 16.3738),
        (&["以色列", "IL", "ISRAEL", "TEL AVIV", "特拉维夫", "🇮🇱"], "Israel", "IL", "Tel Aviv", "Tel Aviv", 32.0853, 34.7818),
        (&["新西兰", "NZ", "NEW ZEALAND", "AUCKLAND", "奥克兰", "🇳🇿"], "New Zealand", "NZ", "Auckland", "Auckland", -36.8485, 174.7633),
        (&["中国", "国内", "CN", "CHINA", "BEIJING", "北京", "SHANGHAI", "上海", "GUANGZHOU", "广州", "SHENZHEN", "深圳", "🇨🇳"], "China", "CN", "Beijing", "Beijing", 39.9042, 116.4074),
    ];

    for (keywords, country, code, region, city, lat, lon) in PRESETS {
        for kw in *keywords {
            if upper.contains(kw) {
                return Some(GeoLocation {
                    country: country.to_string(),
                    country_code: code.to_string(),
                    region: region.to_string(),
                    city: city.to_string(),
                    latitude: *lat,
                    longitude: *lon,
                });
            }
        }
    }
    None
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
                    match tokio::net::lookup_host(format!("{host}:443")).await {
                        Ok(mut addrs) => addrs.next().map(|a| a.ip().to_string()),
                        Err(_) => None,
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
                let mut client_builder = AsyncClient::builder().timeout(Duration::from_secs(8));
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
    fn heuristic_location_inference_works() {
        let hk = infer_location_from_name("L1|香港优化01|3x").expect("HK inferred");
        assert_eq!(hk.country_code, "HK");

        let jp = infer_location_from_name("JP-Tokyo-HighSpeed").expect("JP inferred");
        assert_eq!(jp.country_code, "JP");

        let us = infer_location_from_name("🇺🇸 US Los Angeles 05").expect("US inferred");
        assert_eq!(us.country_code, "US");
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
