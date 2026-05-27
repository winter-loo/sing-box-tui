#!/usr/bin/env node

import fs from "fs";
import path from "path";
import YAML from "yaml";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));

const sourcePath =
  process.argv[2] || path.join(__dirname, "clash_proxies.txt");
const outputPath =
  process.argv[3] || path.join(__dirname, "sing_box_nodes.json");

function buildTls(entry) {
  const enabled = Boolean(entry.tls || entry.sni || entry.servername);
  if (!enabled && entry["skip-cert-verify"] == null) {
    return undefined;
  }

  const tls = {
    enabled: true,
  };
  if (entry.sni) {
    tls.server_name = entry.sni;
  }
  if (entry.servername) {
    tls.server_name = entry.servername;
  }
  if (entry["skip-cert-verify"] != null) {
    tls.insecure = Boolean(entry["skip-cert-verify"]);
  }
  return tls;
}

function buildWsTransport(entry) {
  if (entry.network !== "ws") {
    return undefined;
  }

  const wsOpts = entry["ws-opts"] || {};
  const pathValue = wsOpts.path || entry["ws-path"] || "/";
  const headers = {
    ...(wsOpts.headers || {}),
    ...(entry["ws-headers"] || {}),
  };

  const transport = {
    type: "ws",
    path: pathValue,
  };
  if (Object.keys(headers).length > 0) {
    transport.headers = headers;
  }
  return transport;
}

function buildVlessTls(entry) {
  const tls = buildTls(entry);
  if (!tls && !entry["reality-opts"] && !entry["client-fingerprint"]) {
    return undefined;
  }

  const out = tls || { enabled: true };
  if (entry["client-fingerprint"]) {
    out.utls = {
      enabled: true,
      fingerprint: entry["client-fingerprint"],
    };
  }
  if (entry["reality-opts"]) {
    out.reality = {
      enabled: true,
      public_key: entry["reality-opts"]["public-key"],
      short_id: entry["reality-opts"]["short-id"] || "",
    };
  }
  return out;
}

function convertProxy(entry) {
  const base = {
    type: undefined,
    tag: entry.name,
    server: entry.server,
    server_port: entry.port,
  };

  switch (entry.type) {
    case "hysteria2": {
      const outbound = {
        ...base,
        type: "hysteria2",
        password: entry.password,
        up_mbps: entry.up,
        down_mbps: entry.down,
        tls: buildTls(entry),
      };
      if (entry.ports) {
        delete outbound.server_port;
        outbound.server_ports = [String(entry.ports).replace(/-/g, ":")];
      }
      return outbound;
    }
    case "trojan": {
      return {
        ...base,
        type: "trojan",
        password: entry.password,
        tls: buildTls(entry),
        ...(buildWsTransport(entry) ? { transport: buildWsTransport(entry) } : {}),
      };
    }
    case "vmess": {
      return {
        ...base,
        type: "vmess",
        uuid: entry.uuid,
        security: entry.cipher || "auto",
        alter_id: entry.alterId ?? 0,
        ...(buildTls(entry) ? { tls: buildTls(entry) } : {}),
        ...(buildWsTransport(entry) ? { transport: buildWsTransport(entry) } : {}),
      };
    }
    case "ss": {
      return {
        ...base,
        type: "shadowsocks",
        method: entry.cipher,
        password: entry.password,
      };
    }
    case "vless": {
      return {
        ...base,
        type: "vless",
        uuid: entry.uuid,
        flow: entry.flow,
        ...(buildVlessTls(entry) ? { tls: buildVlessTls(entry) } : {}),
        ...(buildWsTransport(entry) ? { transport: buildWsTransport(entry) } : {}),
      };
    }
    default:
      throw new Error(`Unsupported proxy type: ${entry.type}`);
  }
}

const sourceText = fs.readFileSync(sourcePath, "utf8");
const clashConfig = YAML.parse(sourceText);
const proxies = Array.isArray(clashConfig?.proxies) ? clashConfig.proxies : [];
const converted = proxies.map(convertProxy);

fs.writeFileSync(outputPath, JSON.stringify(converted, null, 2) + "\n");
console.log(
  JSON.stringify(
    {
      source: sourcePath,
      output: outputPath,
      proxies: proxies.length,
      types: [...new Set(proxies.map((entry) => entry.type))].sort(),
    },
    null,
    2,
  ),
);
