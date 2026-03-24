use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) fn is_metadata_entry(entry: &ClashProxy) -> bool {
    ["剩余流量", "距离下次重置剩余", "套餐到期"]
        .iter()
        .any(|marker| entry.name.contains(marker))
}

pub(crate) fn convert_clash_proxy(entry: ClashProxy) -> Result<Value> {
    match entry.kind.as_str() {
        "hysteria2" => {
            let mut outbound = json!({
                "type": "hysteria2",
                "tag": entry.name,
                "server": entry.server,
                "password": entry.password,
                "up_mbps": entry.up,
                "down_mbps": entry.down,
                "tls": build_tls(&entry),
            });
            if let Some(ports) = entry.ports.clone() {
                outbound["server_ports"] = json!([ports.replace('-', ":")]);
            } else {
                outbound["server_port"] = json!(entry.port);
            }
            Ok(outbound)
        }
        "trojan" => Ok(json!({
            "type": "trojan",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "password": entry.password,
            "tls": build_tls(&entry),
            "transport": build_transport(&entry),
        })),
        "vmess" => Ok(json!({
            "type": "vmess",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "uuid": entry.uuid,
            "security": entry.cipher.clone().unwrap_or_else(|| "auto".to_string()),
            "alter_id": entry.alter_id.unwrap_or(0),
            "tls": build_tls(&entry),
            "transport": build_transport(&entry),
        })),
        "ss" => Ok(json!({
            "type": "shadowsocks",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "method": entry.cipher,
            "password": entry.password,
        })),
        "vless" => Ok(json!({
            "type": "vless",
            "tag": entry.name,
            "server": entry.server,
            "server_port": entry.port,
            "uuid": entry.uuid,
            "flow": entry.flow,
            "tls": build_vless_tls(&entry),
            "transport": build_transport(&entry),
        })),
        other => bail!("unsupported Clash proxy type: {other}"),
    }
}

fn build_tls(entry: &ClashProxy) -> Value {
    let server_name = entry
        .sni
        .clone()
        .or_else(|| entry.server_name.clone())
        .unwrap_or_default();
    let enabled = entry.tls.unwrap_or(false)
        || !server_name.is_empty()
        || entry.skip_cert_verify.unwrap_or(false);
    if !enabled {
        return Value::Null;
    }

    let mut value = json!({
        "enabled": true,
    });
    if !server_name.is_empty() {
        value["server_name"] = Value::String(server_name);
    }
    if entry.skip_cert_verify.unwrap_or(false) {
        value["insecure"] = Value::Bool(true);
    }
    value
}

fn build_vless_tls(entry: &ClashProxy) -> Value {
    let mut tls = build_tls(entry);
    if tls.is_null() {
        tls = json!({ "enabled": true });
    }

    if let Some(fingerprint) = entry.client_fingerprint.as_ref() {
        tls["utls"] = json!({
            "enabled": true,
            "fingerprint": fingerprint,
        });
    }

    if let Some(reality) = entry.reality_opts.as_ref() {
        tls["reality"] = json!({
            "enabled": true,
            "public_key": reality.public_key,
            "short_id": reality.short_id.clone().unwrap_or_default(),
        });
    }

    tls
}

fn build_transport(entry: &ClashProxy) -> Value {
    if entry.network.as_deref() != Some("ws") {
        return Value::Null;
    }

    let mut headers = serde_json::Map::new();
    if let Some(ws_opts) = entry.ws_opts.as_ref() {
        for (key, value) in &ws_opts.headers {
            headers.insert(key.clone(), Value::String(value.clone()));
        }
    }
    for (key, value) in &entry.ws_headers {
        headers.insert(key.clone(), Value::String(value.clone()));
    }

    let path = entry
        .ws_opts
        .as_ref()
        .and_then(|opts| opts.path.clone())
        .or_else(|| entry.ws_path.clone())
        .unwrap_or_else(|| "/".to_string());

    let mut transport = json!({
        "type": "ws",
        "path": path,
    });
    if !headers.is_empty() {
        transport["headers"] = Value::Object(headers);
    }
    transport
}

#[derive(Deserialize)]
pub(crate) struct ClashConfig {
    #[serde(default)]
    pub(crate) proxies: Vec<ClashProxy>,
}

#[derive(Deserialize)]
pub(crate) struct ClashProxy {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    server: String,
    port: u16,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    cipher: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    flow: Option<String>,
    #[serde(rename = "alterId", default)]
    alter_id: Option<u32>,
    #[serde(default)]
    tls: Option<bool>,
    #[serde(rename = "skip-cert-verify", default)]
    skip_cert_verify: Option<bool>,
    #[serde(default)]
    sni: Option<String>,
    #[serde(rename = "servername", default)]
    server_name: Option<String>,
    #[serde(default)]
    network: Option<String>,
    #[serde(rename = "ws-opts", default)]
    ws_opts: Option<ClashWsOpts>,
    #[serde(rename = "ws-path", default)]
    ws_path: Option<String>,
    #[serde(rename = "ws-headers", default)]
    ws_headers: std::collections::BTreeMap<String, String>,
    #[serde(rename = "client-fingerprint", default)]
    client_fingerprint: Option<String>,
    #[serde(rename = "reality-opts", default)]
    reality_opts: Option<ClashRealityOpts>,
    #[serde(default)]
    up: Option<u64>,
    #[serde(default)]
    down: Option<u64>,
    #[serde(default)]
    ports: Option<String>,
}

#[derive(Deserialize)]
struct ClashRealityOpts {
    #[serde(rename = "public-key")]
    public_key: String,
    #[serde(rename = "short-id", default)]
    short_id: Option<String>,
}

#[derive(Deserialize)]
struct ClashWsOpts {
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
}
