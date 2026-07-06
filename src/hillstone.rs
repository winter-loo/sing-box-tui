use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::io::ErrorKind;
use std::io::{self, Read, Write};
use std::net::{
    Ipv4Addr, Shutdown, SocketAddrV4, TcpListener, TcpStream, ToSocketAddrs, UdpSocket,
};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use aes::Aes128;
use anyhow::{Context, Result, bail};
use cbc::cipher::{BlockDecryptMut, BlockEncryptMut, KeyIvInit, block_padding::NoPadding};
use hmac::{Hmac, Mac};
use md5::Md5;
use native_tls::{TlsConnector, TlsStream};
use sha1::{Digest, Sha1};

use crate::config::{HillstoneRouteTableOptions, run_hillstone_route_table_config};

type Aes128CbcEnc = cbc::Encryptor<Aes128>;
type Aes128CbcDec = cbc::Decryptor<Aes128>;
type HmacMd5 = Hmac<Md5>;

pub(crate) struct HillstoneProbeOptions {
    pub(crate) server: String,
    pub(crate) port: u16,
    pub(crate) username: String,
    pub(crate) password: Option<String>,
    pub(crate) password_env: Option<String>,
    pub(crate) password_stdin: bool,
    pub(crate) host_id: Option<String>,
    pub(crate) host_name: Option<String>,
    pub(crate) client_version: String,
    pub(crate) timeout_secs: u64,
    pub(crate) verify_server_cert: bool,
    pub(crate) stop_before_new_key: bool,
    pub(crate) udp_icmp_probe: bool,
    pub(crate) udp_tcp_probe: Option<String>,
    pub(crate) udp_http_get: Option<String>,
    pub(crate) udp_http_proxy: Option<String>,
    pub(crate) route_config_path: PathBuf,
    pub(crate) apply_routes: bool,
    pub(crate) apply_routes_for_proxy: bool,
    pub(crate) route_proxy: Option<SocketAddrV4>,
    pub(crate) event_sink: Option<Arc<dyn HillstoneEventSink>>,
    pub(crate) shutdown: Option<Arc<AtomicBool>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct HillstoneNetworkInfo {
    pub(crate) client_ipv4: Option<Ipv4Addr>,
    pub(crate) prefix_len: Option<u32>,
    pub(crate) gateway_ipv4: Option<Ipv4Addr>,
    pub(crate) server_udp_port: Option<u16>,
    pub(crate) dns_ipv4: Vec<Ipv4Addr>,
    pub(crate) route_cidrs: Vec<String>,
    pub(crate) bridge_listen: Option<SocketAddrV4>,
}

pub(crate) trait HillstoneEventSink: Send + Sync {
    fn state_changed(&self, state: &str, message: &str) -> Result<()>;
    fn routes_pushed(&self, info: &HillstoneNetworkInfo) -> Result<()>;
}

pub(crate) fn run_hillstone_probe(options: HillstoneProbeOptions) -> Result<()> {
    let password = read_password(&options)?;
    let host_id = options
        .host_id
        .clone()
        .or_else(load_hillstone_uuid)
        .unwrap_or_else(|| "sing-box-tui-probe".to_string());
    let host_name = options
        .host_name
        .clone()
        .or_else(local_host_name)
        .unwrap_or_else(|| "sing-box-tui".to_string());

    eprintln!(
        "Connecting to {}:{} (tls verification: {})",
        options.server,
        options.port,
        if options.verify_server_cert {
            "enabled"
        } else {
            "disabled"
        }
    );
    emit_hillstone_state(&options, "connecting", "connecting to gateway")?;

    let mut stream = connect_tls(&options)?;
    eprintln!("TLS connected");

    send_auth(
        &mut stream,
        &options.username,
        &password,
        &options.client_version,
        &host_id,
        &host_name,
    )?;
    eprintln!("AUTH accepted");
    emit_hillstone_state(&options, "connecting", "authentication accepted")?;

    send_client_info(&mut stream, &options.server, options.port)?;
    eprintln!("CLNT_INFO accepted");

    let network = wait_network_setup(&mut stream)?;
    eprintln!("Network setup:");
    if let Some(ip) = network.client_private_ipv4 {
        eprintln!("  client_ipv4: {ip}/{}", network.prefix_len.unwrap_or(32));
    }
    if let Some(gateway) = network.server_private_ipv4 {
        eprintln!("  gateway_ipv4: {gateway}");
    }
    if let Some(port) = network.server_udp_port {
        eprintln!("  server_udp_port: {port}");
    }
    if !network.dns_ipv4.is_empty() {
        eprintln!("  dns_ipv4: {}", join_ipv4(&network.dns_ipv4));
    }
    if !network.route_ipv4.is_empty() {
        eprintln!("  route_ipv4_raw_len: {}", network.route_ipv4.len());
        for route in decode_ipv4_routes(&network.route_ipv4) {
            eprintln!("  route_ipv4: {route}");
        }
    }
    emit_hillstone_routes(&options, &network)?;
    let routes_applied = apply_pushed_routes_to_config(&options, &network)?;

    if options.stop_before_new_key {
        send_logout(&mut stream)?;
        eprintln!("Stopped before NEW_KEY as requested");
        emit_hillstone_state(&options, "disconnected", "stopped before key negotiation")?;
        return Ok(());
    }

    let key_summary = send_new_key(&mut stream)?;
    eprintln!("NEW_KEY accepted:");
    eprintln!(
        "  enc_alg: {}",
        describe_algorithm(key_summary.enc_alg, encryption_algorithm_name)
    );
    eprintln!(
        "  auth_alg: {}",
        describe_algorithm(key_summary.auth_alg, auth_algorithm_name)
    );
    eprintln!(
        "  ipcomp_alg: {}",
        describe_algorithm(key_summary.ipcomp_alg, ipcomp_algorithm_name)
    );
    if let Some(spi) = key_summary.outbound_spi {
        eprintln!("  outbound_spi: 0x{spi:08x}");
    }
    if key_summary.session_id_present {
        eprintln!("  session_id: <redacted>");
    }
    emit_hillstone_state(&options, "connected", "data tunnel ready")?;

    let shutdown = if options.udp_http_proxy.is_some() {
        Some(match &options.shutdown {
            Some(shutdown) => Arc::clone(shutdown),
            None => install_shutdown_handler()?,
        })
    } else {
        None
    };
    let probe_result = if options.udp_icmp_probe {
        run_udp_icmp_probe(&options, &network, &key_summary)
    } else if let Some(target) = &options.udp_tcp_probe {
        run_udp_tcp_probe(&options, &network, &key_summary, target)
    } else if let Some(url) = &options.udp_http_get {
        run_udp_http_get(&options, &network, &key_summary, url)
    } else if let Some(listen) = &options.udp_http_proxy {
        run_udp_http_proxy(
            &options,
            &network,
            &key_summary,
            listen,
            shutdown.expect("UDP HTTP proxy shutdown flag is installed"),
        )
    } else {
        Ok(())
    };

    let logout_result = send_logout(&mut stream);
    if let Err(error) = probe_result {
        if let Err(logout_error) = logout_result {
            eprintln!(
                "warning: failed to send Hillstone logout after UDP probe error: {logout_error:#}"
            );
        }
        return Err(error);
    }
    logout_result?;
    emit_hillstone_state(&options, "disconnected", "logout sent")?;
    let route_status = if routes_applied {
        "routes were applied to config"
    } else {
        "no routes were changed"
    };
    if options.udp_icmp_probe {
        eprintln!("Probe complete; one UDP ESP ICMP probe was attempted and {route_status}");
    } else if options.udp_tcp_probe.is_some() {
        eprintln!("Probe complete; one UDP ESP TCP SYN probe succeeded and {route_status}");
    } else if options.udp_http_get.is_some() {
        eprintln!("Probe complete; one UDP ESP HTTP GET succeeded and {route_status}");
    } else if options.udp_http_proxy.is_some() {
        eprintln!("Probe complete; UDP ESP HTTP proxy exited and {route_status}");
    } else {
        eprintln!("Probe complete; no UDP data tunnel was opened and {route_status}");
    }
    Ok(())
}

fn emit_hillstone_state(options: &HillstoneProbeOptions, state: &str, message: &str) -> Result<()> {
    if let Some(sink) = &options.event_sink {
        sink.state_changed(state, message)?;
    }
    Ok(())
}

fn emit_hillstone_routes(options: &HillstoneProbeOptions, network: &NetworkSetup) -> Result<()> {
    let Some(sink) = &options.event_sink else {
        return Ok(());
    };
    let bridge_listen = hillstone_route_proxy(options)?;
    // SET_ROUTE is the only authoritative source we currently have for the intranet ranges.
    // Emitting it before local route application lets the TUI own sing-box config changes while
    // the Hillstone provider process stays focused on protocol/session handling.
    sink.routes_pushed(&HillstoneNetworkInfo {
        client_ipv4: network.client_private_ipv4,
        prefix_len: network.prefix_len,
        gateway_ipv4: network.server_private_ipv4,
        server_udp_port: network.server_udp_port,
        dns_ipv4: network.dns_ipv4.clone(),
        route_cidrs: decode_ipv4_route_cidrs(&network.route_ipv4),
        bridge_listen,
    })?;
    Ok(())
}

fn install_shutdown_handler() -> Result<Arc<AtomicBool>> {
    let shutdown = Arc::new(AtomicBool::new(false));
    let handler_shutdown = Arc::clone(&shutdown);
    ctrlc::set_handler(move || {
        handler_shutdown.store(true, Ordering::SeqCst);
        eprintln!("Shutdown requested; closing Hillstone SSL VPN connection...");
    })
    .context("failed to install shutdown handler")?;
    Ok(shutdown)
}

fn apply_pushed_routes_to_config(
    options: &HillstoneProbeOptions,
    network: &NetworkSetup,
) -> Result<bool> {
    if !options.apply_routes
        && !(options.apply_routes_for_proxy && options.udp_http_proxy.is_some())
    {
        return Ok(false);
    }

    let cidrs = decode_ipv4_route_cidrs(&network.route_ipv4);
    if cidrs.is_empty() {
        eprintln!("No parsable Hillstone route table was pushed; config not changed");
        return Ok(false);
    }
    let proxy = hillstone_route_proxy(options)?.context(
        "Hillstone route application requires --route-proxy <IP:PORT> or --udp-http-proxy <IP:PORT>",
    )?;

    // The gateway pushes SET_ROUTE before the ESP data tunnel starts. Writing those CIDRs
    // here lets sing-box keep its normal mixed/system-proxy entry point while each matched
    // intranet destination is internally rewritten to the local Hillstone HTTP bridge.
    run_hillstone_route_table_config(
        &options.route_config_path,
        None,
        true,
        HillstoneRouteTableOptions {
            cidrs: cidrs.clone(),
            proxy,
        },
    )?;
    eprintln!(
        "Applied Hillstone route table to {} via {}: {}",
        options.route_config_path.display(),
        proxy,
        cidrs.join(", ")
    );
    Ok(true)
}

fn hillstone_route_proxy(options: &HillstoneProbeOptions) -> Result<Option<SocketAddrV4>> {
    if let Some(proxy) = options.route_proxy {
        return Ok(Some(proxy));
    }
    options
        .udp_http_proxy
        .as_deref()
        .map(|value| {
            value.parse::<SocketAddrV4>().with_context(|| {
                format!("--udp-http-proxy listen address must be IPv4:PORT, got {value}")
            })
        })
        .transpose()
}

fn connect_tls(options: &HillstoneProbeOptions) -> Result<TlsStream<TcpStream>> {
    let timeout = Duration::from_secs(options.timeout_secs);
    let addrs = (options.server.as_str(), options.port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {}:{}", options.server, options.port))?
        .collect::<Vec<_>>();
    if addrs.is_empty() {
        bail!(
            "{}:{} resolved to no addresses",
            options.server,
            options.port
        );
    }

    let mut last_error = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(stream) => {
                stream
                    .set_read_timeout(Some(timeout))
                    .context("failed to set read timeout")?;
                stream
                    .set_write_timeout(Some(timeout))
                    .context("failed to set write timeout")?;
                let connector = TlsConnector::builder()
                    .danger_accept_invalid_certs(!options.verify_server_cert)
                    .danger_accept_invalid_hostnames(!options.verify_server_cert)
                    .build()
                    .context("failed to build TLS connector")?;
                return connector
                    .connect(&options.server, stream)
                    .context("failed to complete TLS handshake");
            }
            Err(error) => last_error = Some(error),
        }
    }

    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("failed to connect"))
        .context("failed to connect to Hillstone gateway"))
}

fn send_auth(
    stream: &mut TlsStream<TcpStream>,
    username: &str,
    password: &str,
    client_version: &str,
    host_id: &str,
    host_name: &str,
) -> Result<()> {
    let mut message = Message::new(MessageType::Auth);
    message.push_u16(Payload::AuthType, 1);
    message.push_str(Payload::Username, username);
    message.push_str(Payload::Password, password);
    message.push_str(Payload::ClientVer, client_version);
    message.push_str(Payload::HostId, host_id);
    message.push_str(Payload::HostName, host_name);
    send_message(stream, message)?;

    let frame = read_non_empty_frame(stream)?;
    print_frame_summary("AUTH response", &frame);
    ensure_ok_status(&frame, "AUTH")
}

fn send_client_info(stream: &mut TlsStream<TcpStream>, server: &str, port: u16) -> Result<()> {
    let server_ip = resolve_ipv4(server)?;
    let client_ip = local_ipv4_for_remote(server, port)?;
    let mut message = Message::new(MessageType::ClientInfo);
    message.push_ipv4(Payload::ClientPublicIpv4, client_ip);
    message.push_ipv4(Payload::ServerPublicIpv4, server_ip);
    send_message(stream, message)?;

    let frame = read_non_empty_frame(stream)?;
    print_frame_summary("CLNT_INFO response", &frame);
    ensure_ok_status(&frame, "CLNT_INFO")
}

fn wait_network_setup(stream: &mut TlsStream<TcpStream>) -> Result<NetworkSetup> {
    let mut setup = NetworkSetup::default();
    loop {
        let frame = read_non_empty_frame(stream)?;
        print_frame_summary("network frame", &frame);
        ensure_ok_status(&frame, "network setup")?;

        match frame.message_type {
            MessageType::SetIp => {
                setup.server_udp_port = frame.payload_u16(Payload::ServerUdpPort);
                setup.client_private_ipv4 = frame.payload_ipv4(Payload::ClientPrivateIpv4);
                setup.server_private_ipv4 = frame.payload_ipv4(Payload::ServerPrivateIpv4);
                setup.prefix_len = frame
                    .payload_ipv4(Payload::NetmaskIpv4)
                    .map(netmask_prefix_len);
                setup.dns_ipv4 = decode_ipv4_list(
                    frame
                        .payloads
                        .get(&(Payload::DnsIpv4 as u16))
                        .map(Vec::as_slice)
                        .unwrap_or_default(),
                );
            }
            MessageType::SetRoute => {
                if let Some(routes) = frame.payloads.get(&(Payload::RouteIpv4 as u16)) {
                    setup.route_ipv4 = routes.clone();
                }
            }
            MessageType::KeyDone => return Ok(setup),
            MessageType::ServerDisconnect => bail!("server disconnected during network setup"),
            other => {
                eprintln!("  ignored_message: {}", other.name());
            }
        }
    }
}

fn send_new_key(stream: &mut TlsStream<TcpStream>) -> Result<NewKeySummary> {
    let mut key_material = [0_u8; 0x30];
    fill_random(&mut key_material);
    let inbound_spi = random_u32();
    let inbound_cpi = random_u16();

    let mut message = Message::new(MessageType::NewKey);
    message.push_u16(Payload::KeyExchangeMode, 3);
    message.push_bytes(Payload::Keymat, &key_material);
    message.push_u32(Payload::Spi, inbound_spi);
    message.push_u16(Payload::IpcompCpi, inbound_cpi);
    send_message(stream, message)?;

    let frame = read_non_empty_frame(stream)?;
    print_frame_summary("NEW_KEY response", &frame);
    ensure_ok_status(&frame, "NEW_KEY")?;

    Ok(NewKeySummary {
        enc_alg: frame.payload_u16(Payload::EncAlg),
        auth_alg: frame.payload_u16(Payload::AuthAlg),
        ipcomp_alg: frame.payload_u16(Payload::IpcompAlg),
        outbound_spi: frame.payload_u32(Payload::Spi),
        inbound_spi,
        key_material,
        session_id_present: frame.payloads.contains_key(&(Payload::SessionId as u16)),
    })
}

fn send_logout(stream: &mut TlsStream<TcpStream>) -> Result<()> {
    let mut message = Message::new(MessageType::ClientLogout);
    message.push_u16(Payload::Disconnect, 0);
    send_message(stream, message)
}

fn run_udp_icmp_probe(
    options: &HillstoneProbeOptions,
    network: &NetworkSetup,
    key_summary: &NewKeySummary,
) -> Result<()> {
    let client_ip = network
        .client_private_ipv4
        .context("UDP ICMP probe requires SET_IP client private IPv4")?;
    let gateway_ip = network
        .server_private_ipv4
        .context("UDP ICMP probe requires SET_IP server private IPv4")?;
    let server_udp_port = network
        .server_udp_port
        .context("UDP ICMP probe requires SET_IP server UDP port")?;
    let server_ip = resolve_ipv4(&options.server)?;
    let timeout = Duration::from_secs(options.timeout_secs);
    let mut esp = EspSession::from_new_key(key_summary)?;

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind local UDP socket for Hillstone ESP probe")?;
    socket
        .set_read_timeout(Some(timeout))
        .context("failed to set UDP read timeout")?;
    socket
        .set_write_timeout(Some(timeout))
        .context("failed to set UDP write timeout")?;

    let echo_id = random_u16();
    let echo_request = build_icmp_echo_request(echo_id, 1, b"sing-box-tui hillstone udp probe");
    let inner_packet = build_ipv4_packet(client_ip, gateway_ip, IPPROTO_ICMP, &echo_request)?;
    let esp_packet = esp.encap_ipv4(&inner_packet)?;

    eprintln!("UDP ESP probe:");
    eprintln!("  target: {server_ip}:{server_udp_port}");
    eprintln!("  inner_ipv4: {client_ip} -> {gateway_ip} icmp_echo_id=0x{echo_id:04x}");
    socket
        .send_to(&esp_packet, (server_ip, server_udp_port))
        .context("failed to send UDP ESP probe")?;

    let mut buffer = [0_u8; 4096];
    match socket.recv_from(&mut buffer) {
        Ok((size, source)) => {
            eprintln!("  udp_response_bytes: {size} from {source}");
            let inner_response = esp
                .decap_ipv4(&buffer[..size])
                .context("received UDP response but could not decrypt ESP packet")?;
            eprintln!(
                "  decrypted_inner: {}",
                describe_ipv4_packet(&inner_response)
            );
        }
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
            eprintln!("  udp_response: timed out after {}s", options.timeout_secs);
        }
        Err(error) => return Err(error).context("failed to receive UDP ESP probe response"),
    }

    Ok(())
}

fn run_udp_tcp_probe(
    options: &HillstoneProbeOptions,
    network: &NetworkSetup,
    key_summary: &NewKeySummary,
    target: &str,
) -> Result<()> {
    let client_ip = network
        .client_private_ipv4
        .context("UDP TCP probe requires SET_IP client private IPv4")?;
    let server_udp_port = network
        .server_udp_port
        .context("UDP TCP probe requires SET_IP server UDP port")?;
    let server_ip = resolve_ipv4(&options.server)?;
    let target = parse_tcp_probe_target(target)?;
    let timeout = Duration::from_secs(options.timeout_secs);
    let mut esp = EspSession::from_new_key(key_summary)?;

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind local UDP socket for Hillstone ESP TCP probe")?;
    socket
        .set_read_timeout(Some(timeout))
        .context("failed to set UDP read timeout")?;
    socket
        .set_write_timeout(Some(timeout))
        .context("failed to set UDP write timeout")?;

    let source_port = random_ephemeral_port();
    let sequence = random_u32();
    let tcp_syn = build_tcp_segment(TcpSegmentSpec {
        source_ip: client_ip,
        destination_ip: *target.ip(),
        source_port,
        destination_port: target.port(),
        sequence,
        acknowledgement: 0,
        flags: TCP_FLAG_SYN,
        payload: &[],
    })?;

    eprintln!("UDP ESP TCP probe:");
    eprintln!("  target: {server_ip}:{server_udp_port}");
    eprintln!(
        "  inner_tcp_syn: {client_ip}:{source_port} -> {}:{} seq=0x{sequence:08x}",
        target.ip(),
        target.port()
    );
    let deadline = Instant::now() + timeout;
    let mut attempts = 0_u32;
    let mut buffer = [0_u8; 4096];

    loop {
        attempts += 1;
        let inner_packet = build_ipv4_packet(client_ip, *target.ip(), IPPROTO_TCP, &tcp_syn)?;
        let esp_packet = esp.encap_ipv4(&inner_packet)?;
        socket
            .send_to(&esp_packet, (server_ip, server_udp_port))
            .with_context(|| format!("failed to send UDP ESP TCP probe attempt {attempts}"))?;

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let read_timeout = (deadline - now).min(Duration::from_secs(1));
        socket
            .set_read_timeout(Some(read_timeout))
            .context("failed to update UDP read timeout")?;

        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                eprintln!("  udp_response_bytes: {size} from {source} after attempt {attempts}");
                let inner_response = esp
                    .decap_ipv4(&buffer[..size])
                    .context("received UDP response but could not decrypt ESP packet")?;
                eprintln!(
                    "  decrypted_inner: {}",
                    describe_ipv4_packet(&inner_response)
                );
                let outcome = tcp_probe_outcome(
                    &inner_response,
                    client_ip,
                    *target.ip(),
                    source_port,
                    target.port(),
                    sequence,
                )?;
                eprintln!("  tcp_probe_result: {}", outcome.description());
                if !outcome.is_success() {
                    bail!("UDP ESP TCP probe did not receive a SYN-ACK from {target}");
                }
                return Ok(());
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(error) => {
                return Err(error).context("failed to receive UDP ESP TCP probe response");
            }
        }
    }

    bail!(
        "UDP ESP TCP probe to {target} timed out after {}s and {attempts} attempts",
        options.timeout_secs
    )
}

fn run_udp_http_get(
    options: &HillstoneProbeOptions,
    network: &NetworkSetup,
    key_summary: &NewKeySummary,
    url: &str,
) -> Result<()> {
    let request = parse_http_get_url(url)?;
    let client_ip = network
        .client_private_ipv4
        .context("UDP HTTP GET requires SET_IP client private IPv4")?;
    let server_udp_port = network
        .server_udp_port
        .context("UDP HTTP GET requires SET_IP server UDP port")?;
    let server_ip = resolve_ipv4(&options.server)?;
    let timeout = Duration::from_secs(options.timeout_secs);
    let mut esp = EspSession::from_new_key(key_summary)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind local UDP socket for Hillstone ESP HTTP GET")?;
    socket
        .set_write_timeout(Some(timeout))
        .context("failed to set UDP write timeout")?;

    eprintln!("UDP ESP HTTP GET:");
    eprintln!("  target: {server_ip}:{server_udp_port}");
    eprintln!(
        "  request: http://{}:{}{}",
        request.host, request.port, request.path_with_query
    );

    let http_request = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: sing-box-tui-hillstone-probe\r\nAccept: */*\r\nConnection: close\r\n\r\n",
        request.path_with_query,
        request.host_header()
    );
    let response = http_request_over_esp(
        &socket,
        &mut esp,
        (server_ip, server_udp_port),
        client_ip,
        SocketAddrV4::new(request.host, request.port),
        http_request.as_bytes(),
        timeout,
    )?;
    let status_line = http_status_line(&response).unwrap_or("<missing status line>");
    eprintln!("  http_status: {status_line}");
    eprintln!("  http_response_bytes: {}", response.len());
    if !status_line.starts_with("HTTP/1.1 200") && !status_line.starts_with("HTTP/1.0 200") {
        bail!("UDP ESP HTTP GET returned non-200 status: {status_line}");
    }

    Ok(())
}

fn run_udp_http_proxy(
    options: &HillstoneProbeOptions,
    network: &NetworkSetup,
    key_summary: &NewKeySummary,
    listen: &str,
    shutdown: Arc<AtomicBool>,
) -> Result<()> {
    let listen = listen.parse::<SocketAddrV4>().with_context(|| {
        format!("--udp-http-proxy listen address must be IPv4:PORT, got {listen}")
    })?;
    let client_ip = network
        .client_private_ipv4
        .context("UDP HTTP proxy requires SET_IP client private IPv4")?;
    let gateway_ip = network
        .server_private_ipv4
        .context("UDP HTTP proxy requires SET_IP server private IPv4")?;
    let server_udp_port = network
        .server_udp_port
        .context("UDP HTTP proxy requires SET_IP server UDP port")?;
    let server_ip = resolve_ipv4(&options.server)?;
    let timeout = Duration::from_secs(options.timeout_secs);
    let mut esp = EspSession::from_new_key(key_summary)?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind local UDP socket for Hillstone ESP HTTP proxy")?;
    socket
        .set_write_timeout(Some(timeout))
        .context("failed to set UDP write timeout")?;

    let listener = TcpListener::bind(listen)
        .with_context(|| format!("failed to bind local HTTP proxy listener on {listen}"))?;
    listener
        .set_nonblocking(true)
        .context("failed to set local HTTP proxy listener nonblocking")?;
    eprintln!("UDP ESP HTTP proxy:");
    eprintln!("  listen: http://{listen}");
    eprintln!("  esp_gateway: {server_ip}:{server_udp_port}");
    eprintln!("  note: keep this process running while testing in the browser");

    let mut source_ports = SourcePortAllocator::new();
    let mut next_keepalive = Instant::now() + HILLSTONE_PROXY_KEEPALIVE_INTERVAL;
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut client, _)) => {
                // The keepalive loop needs a nonblocking listener, but macOS can hand accepted
                // sockets to us in nonblocking mode too. That caused Chrome requests to fail as
                // immediate WouldBlock reads and surfaced to the user as 502 Bad Gateway.
                client
                    .set_nonblocking(false)
                    .context("failed to set accepted browser connection blocking")?;
                if let Err(error) = handle_http_proxy_client(
                    &mut client,
                    &socket,
                    &mut esp,
                    (server_ip, server_udp_port),
                    client_ip,
                    listen,
                    &mut source_ports,
                    timeout,
                ) {
                    eprintln!("warning: failed to proxy browser request: {error:#}");
                    let _ = write_simple_http_error(&mut client, 502, "Bad Gateway");
                }
                let _ = client.shutdown(Shutdown::Both);
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock) => {
                if Instant::now() >= next_keepalive {
                    // Manual browser testing showed the local proxy could keep accepting
                    // requests while the Hillstone data tunnel stopped answering new ESP TCP
                    // SYNs. An unsolicited TLS KEEP_ALIVE closes this gateway's control channel,
                    // so the practical idle fix here is data-plane-only: keep the UDP ESP path
                    // warm with a tiny encrypted ICMP packet to the assigned gateway.
                    send_proxy_keepalive(
                        &socket,
                        &mut esp,
                        (server_ip, server_udp_port),
                        client_ip,
                        gateway_ip,
                    )?;
                    next_keepalive = Instant::now() + HILLSTONE_PROXY_KEEPALIVE_INTERVAL;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(error) => {
                eprintln!("warning: failed to accept local browser connection: {error}");
            }
        }
    }
    // Ctrl-C used to kill the process while the Hillstone control channel was still open, leaving
    // the gateway to expire the session on its own. Returning here lets run_hillstone_probe send a
    // CLIENT_LOGOUT frame and release the local bridge port as our own client exits.
    eprintln!("UDP ESP HTTP proxy stopped by local shutdown request");
    Ok(())
}

fn send_proxy_keepalive(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    client_ip: Ipv4Addr,
    gateway_ip: Ipv4Addr,
) -> Result<()> {
    let echo_request = build_icmp_echo_request(random_u16(), 0, b"sing-box-tui keepalive");
    let inner_packet = build_ipv4_packet(client_ip, gateway_ip, IPPROTO_ICMP, &echo_request)?;
    let esp_packet = esp.encap_ipv4(&inner_packet)?;
    socket
        .send_to(&esp_packet, server_endpoint)
        .context("failed to send Hillstone ESP keepalive")?;
    Ok(())
}

fn handle_http_proxy_client(
    client: &mut TcpStream,
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    client_ip: Ipv4Addr,
    listen: SocketAddrV4,
    source_ports: &mut SourcePortAllocator,
    timeout: Duration,
) -> Result<()> {
    let browser_request = read_http_request(client, timeout)?;
    let target = resolve_http_proxy_target(&browser_request, listen)?;
    let proxy_request = build_upstream_http_request(&browser_request, listen, target)?;
    eprintln!(
        "  browser_request: {} {} -> {}",
        proxy_request.method, proxy_request.path, target
    );
    let response = http_request_over_esp_from_source_port(
        socket,
        esp,
        server_endpoint,
        client_ip,
        target,
        source_ports.next(),
        &proxy_request.bytes,
        timeout,
    )?;
    let status_line = http_status_line(&response).unwrap_or("<missing status line>");
    eprintln!("  browser_response: {status_line} bytes={}", response.len());
    let response = rewrite_http_response_for_proxy(&response, target, &proxy_request.browser_base);
    client
        .write_all(&response)
        .context("failed to write proxied response to browser")?;
    Ok(())
}

fn http_request_over_esp(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    client_ip: Ipv4Addr,
    target: SocketAddrV4,
    request: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    let source_port = random_ephemeral_port();
    http_request_over_esp_from_source_port(
        socket,
        esp,
        server_endpoint,
        client_ip,
        target,
        source_port,
        request,
        timeout,
    )
}

fn http_request_over_esp_from_source_port(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    client_ip: Ipv4Addr,
    target: SocketAddrV4,
    source_port: u16,
    request: &[u8],
    timeout: Duration,
) -> Result<Vec<u8>> {
    let client_isn = random_u32();
    eprintln!("  inner_tcp: {client_ip}:{source_port} -> {target} seq=0x{client_isn:08x}");
    let mut state = tcp_handshake_over_esp(
        socket,
        esp,
        server_endpoint,
        TcpPeer {
            client_ip,
            target_ip: *target.ip(),
            source_port,
            destination_port: target.port(),
        },
        client_isn,
        timeout,
    )?;
    send_tcp_payload_over_esp(socket, esp, server_endpoint, &mut state, request)?;
    let response = recv_http_response_over_esp(socket, esp, server_endpoint, &mut state, timeout)?;
    send_tcp_over_esp(
        socket,
        esp,
        server_endpoint,
        state.peer,
        state.client_next_seq,
        state.server_next_seq,
        TCP_FLAG_FIN | TCP_FLAG_ACK,
        &[],
    )?;
    Ok(response)
}

struct SourcePortAllocator {
    next: u16,
}

impl SourcePortAllocator {
    fn new() -> Self {
        Self {
            next: random_ephemeral_port(),
        }
    }

    fn next(&mut self) -> u16 {
        // Browser proxy traffic exposed a real 4-tuple collision problem: delayed FIN/ACK
        // packets from an old ESP flow can arrive while the next request is starting. Walking
        // source ports forward inside one proxy run avoids reusing a recently closed flow.
        let port = self.next;
        self.next = if self.next == u16::MAX {
            49152
        } else {
            self.next + 1
        };
        port
    }
}

fn send_tcp_payload_over_esp(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    state: &mut TcpConnectionState,
    payload: &[u8],
) -> Result<()> {
    for chunk in payload.chunks(1200) {
        send_tcp_over_esp(
            socket,
            esp,
            server_endpoint,
            state.peer,
            state.client_next_seq,
            state.server_next_seq,
            TCP_FLAG_PSH | TCP_FLAG_ACK,
            chunk,
        )?;
        state.client_next_seq = state.client_next_seq.wrapping_add(chunk.len() as u32);
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct TcpPeer {
    client_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
}

struct TcpConnectionState {
    peer: TcpPeer,
    client_next_seq: u32,
    server_next_seq: u32,
}

fn tcp_handshake_over_esp(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    peer: TcpPeer,
    client_isn: u32,
    timeout: Duration,
) -> Result<TcpConnectionState> {
    let syn = build_tcp_segment(TcpSegmentSpec {
        source_ip: peer.client_ip,
        destination_ip: peer.target_ip,
        source_port: peer.source_port,
        destination_port: peer.destination_port,
        sequence: client_isn,
        acknowledgement: 0,
        flags: TCP_FLAG_SYN,
        payload: &[],
    })?;
    let deadline = Instant::now() + timeout;
    let mut attempts = 0_u32;
    let mut buffer = [0_u8; 4096];

    loop {
        attempts += 1;
        send_inner_tcp_packet(socket, esp, server_endpoint, peer, &syn)?;
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        socket
            .set_read_timeout(Some((deadline - now).min(Duration::from_secs(1))))
            .context("failed to update UDP read timeout")?;

        match socket.recv_from(&mut buffer) {
            Ok((size, source)) => {
                let inner_response = esp
                    .decap_ipv4(&buffer[..size])
                    .context("received UDP response but could not decrypt ESP packet")?;
                let parsed = match parse_matching_tcp_packet(&inner_response, peer) {
                    Ok(parsed) => parsed,
                    // The Hillstone tunnel is session-wide, so the UDP socket can also receive
                    // DNS replies and delayed packets from earlier browser requests. Ignoring
                    // non-matching inner packets made the user-space TCP handshake reliable.
                    Err(_) => continue,
                };
                eprintln!(
                    "  tcp_handshake_response_bytes: {size} from {source} after attempt {attempts}"
                );
                eprintln!(
                    "  tcp_handshake_inner: {}",
                    describe_ipv4_packet(&inner_response)
                );
                if parsed.flags & TCP_FLAG_RST != 0 {
                    bail!("target returned TCP RST during HTTP GET handshake");
                }
                if parsed.flags & TCP_FLAG_SYN != 0
                    && parsed.flags & TCP_FLAG_ACK != 0
                    && parsed.acknowledgement == client_isn.wrapping_add(1)
                {
                    return Ok(TcpConnectionState {
                        peer,
                        client_next_seq: client_isn.wrapping_add(1),
                        server_next_seq: parsed.sequence.wrapping_add(1),
                    });
                }
            }
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                continue;
            }
            Err(error) => {
                return Err(error).context("failed to receive UDP ESP TCP handshake response");
            }
        }
    }

    bail!("UDP ESP HTTP GET TCP handshake timed out after {attempts} SYN attempts")
}

fn recv_http_response_over_esp(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    state: &mut TcpConnectionState,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut response = Vec::new();
    let mut out_of_order = BTreeMap::<u32, Vec<u8>>::new();
    let mut buffer = [0_u8; 8192];

    loop {
        let now = Instant::now();
        if now >= deadline {
            if !response.is_empty() && http_status_line(&response).is_some() {
                return Ok(response);
            }
            bail!("UDP ESP HTTP GET timed out waiting for response");
        }
        socket
            .set_read_timeout(Some((deadline - now).min(Duration::from_secs(2))))
            .context("failed to update UDP read timeout")?;

        let (size, source) = match socket.recv_from(&mut buffer) {
            Ok(value) => value,
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                if !response.is_empty() && http_status_line(&response).is_some() {
                    return Ok(response);
                }
                continue;
            }
            Err(error) => return Err(error).context("failed to receive UDP ESP HTTP response"),
        };
        let inner_response = esp
            .decap_ipv4(&buffer[..size])
            .with_context(|| format!("failed to decrypt UDP ESP HTTP response from {source}"))?;
        let parsed = match parse_matching_tcp_packet(&inner_response, state.peer) {
            Ok(parsed) => parsed,
            // See the handshake path above: the ESP stream is not scoped to this one HTTP
            // request, so unrelated inner packets must be treated as background noise.
            Err(_) => continue,
        };
        if parsed.flags & TCP_FLAG_RST != 0 {
            bail!("target returned TCP RST during HTTP GET response");
        }

        let mut ack_needed = false;
        if !parsed.payload.is_empty() {
            if parsed.sequence == state.server_next_seq {
                response.extend_from_slice(&parsed.payload);
                state.server_next_seq = state
                    .server_next_seq
                    .wrapping_add(parsed.payload.len() as u32);
                while let Some(payload) = out_of_order.remove(&state.server_next_seq) {
                    state.server_next_seq =
                        state.server_next_seq.wrapping_add(payload.len() as u32);
                    response.extend_from_slice(&payload);
                }
            } else if parsed.sequence.wrapping_sub(state.server_next_seq) < 1_000_000 {
                out_of_order
                    .entry(parsed.sequence)
                    .or_insert_with(|| parsed.payload.clone());
            }
            ack_needed = true;
        }

        if parsed.flags & TCP_FLAG_FIN != 0 {
            state.server_next_seq = state.server_next_seq.wrapping_add(1);
            ack_needed = true;
        }

        if ack_needed {
            send_tcp_over_esp(
                socket,
                esp,
                server_endpoint,
                state.peer,
                state.client_next_seq,
                state.server_next_seq,
                TCP_FLAG_ACK,
                &[],
            )?;
        }

        if parsed.flags & TCP_FLAG_FIN != 0 || http_response_has_complete_body(&response) {
            return Ok(response);
        }
    }
}

fn send_tcp_over_esp(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    peer: TcpPeer,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    payload: &[u8],
) -> Result<()> {
    let segment = build_tcp_segment(TcpSegmentSpec {
        source_ip: peer.client_ip,
        destination_ip: peer.target_ip,
        source_port: peer.source_port,
        destination_port: peer.destination_port,
        sequence,
        acknowledgement,
        flags,
        payload,
    })?;
    send_inner_tcp_packet(socket, esp, server_endpoint, peer, &segment)
}

fn send_inner_tcp_packet(
    socket: &UdpSocket,
    esp: &mut EspSession,
    server_endpoint: (Ipv4Addr, u16),
    peer: TcpPeer,
    segment: &[u8],
) -> Result<()> {
    let inner_packet = build_ipv4_packet(peer.client_ip, peer.target_ip, IPPROTO_TCP, segment)?;
    let esp_packet = esp.encap_ipv4(&inner_packet)?;
    socket
        .send_to(&esp_packet, server_endpoint)
        .context("failed to send UDP ESP TCP packet")?;
    Ok(())
}

fn parse_matching_tcp_packet(packet: &[u8], peer: TcpPeer) -> Result<ParsedTcpPacket> {
    let parsed = parse_ipv4_tcp_packet(packet)?;
    if parsed.source_ip != peer.target_ip
        || parsed.destination_ip != peer.client_ip
        || parsed.source_port != peer.destination_port
        || parsed.destination_port != peer.source_port
    {
        bail!(
            "received unrelated TCP packet over ESP: {}",
            describe_ipv4_packet(packet)
        );
    }
    Ok(parsed)
}

const IPPROTO_ICMP: u8 = 1;
const IPPROTO_TCP: u8 = 6;
const ESP_NEXT_HEADER_IPV4: u8 = 4;
const IPV4_HEADER_LEN: usize = 20;
const TCP_HEADER_LEN: usize = 20;
const AES_BLOCK_SIZE: usize = 16;
const AES_128_KEY_LEN: usize = 16;
const HMAC_MD5_KEY_LEN: usize = 16;
const HMAC_MD5_96_LEN: usize = 12;
const HILLSTONE_AES128_CBC: u16 = 12;
const HILLSTONE_HMAC_MD5_96: u16 = 1;
const HILLSTONE_IPCOMP_NONE: u16 = 0;
const HILLSTONE_PROXY_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const TCP_FLAG_FIN: u8 = 0x01;
const TCP_FLAG_SYN: u8 = 0x02;
const TCP_FLAG_RST: u8 = 0x04;
const TCP_FLAG_PSH: u8 = 0x08;
const TCP_FLAG_ACK: u8 = 0x10;

struct EspSession {
    outbound_spi: u32,
    inbound_spi: u32,
    outbound_auth_key: Vec<u8>,
    outbound_crypt_key: Vec<u8>,
    inbound_auth_key: Vec<u8>,
    inbound_crypt_key: Vec<u8>,
    iv: Vec<u8>,
    sequence: u32,
}

impl EspSession {
    fn from_new_key(summary: &NewKeySummary) -> Result<Self> {
        let enc_alg = summary
            .enc_alg
            .context("NEW_KEY response is missing ENC_ALG")?;
        let auth_alg = summary
            .auth_alg
            .context("NEW_KEY response is missing AUTH_ALG")?;
        let ipcomp_alg = summary
            .ipcomp_alg
            .context("NEW_KEY response is missing IPCOMP_ALG")?;
        if enc_alg != HILLSTONE_AES128_CBC {
            bail!(
                "UDP ICMP probe currently supports only AES-128-CBC data encryption, got {}",
                describe_algorithm(Some(enc_alg), encryption_algorithm_name)
            );
        }
        if auth_alg != HILLSTONE_HMAC_MD5_96 {
            bail!(
                "UDP ICMP probe currently supports only HMAC-MD5-96 ESP authentication, got {}",
                describe_algorithm(Some(auth_alg), auth_algorithm_name)
            );
        }
        if ipcomp_alg != HILLSTONE_IPCOMP_NONE {
            bail!(
                "UDP ICMP probe currently supports only no IPComp, got {}",
                describe_algorithm(Some(ipcomp_alg), ipcomp_algorithm_name)
            );
        }

        let outbound_spi = summary
            .outbound_spi
            .context("NEW_KEY response is missing outbound SPI")?;
        let mut offset = 0;
        let expanded = expand_key_material(&summary.key_material);
        let outbound_auth_key = read_key_slice(&expanded, &mut offset, HMAC_MD5_KEY_LEN)?;
        let outbound_crypt_key = read_key_slice(&expanded, &mut offset, AES_128_KEY_LEN)?;
        let inbound_auth_key = read_key_slice(&expanded, &mut offset, HMAC_MD5_KEY_LEN)?;
        let inbound_crypt_key = read_key_slice(&expanded, &mut offset, AES_128_KEY_LEN)?;
        let iv = summary.key_material[..AES_BLOCK_SIZE].to_vec();

        Ok(Self {
            outbound_spi,
            inbound_spi: summary.inbound_spi,
            outbound_auth_key,
            outbound_crypt_key,
            inbound_auth_key,
            inbound_crypt_key,
            iv,
            sequence: 1,
        })
    }

    fn encap_ipv4(&mut self, inner_packet: &[u8]) -> Result<Vec<u8>> {
        if inner_packet.len() < IPV4_HEADER_LEN || inner_packet[0] >> 4 != 4 {
            bail!("ESP encapsulation requires an inner IPv4 packet");
        }

        let mut plaintext = inner_packet.to_vec();
        let pad_len = (AES_BLOCK_SIZE - ((plaintext.len() + 2) % AES_BLOCK_SIZE)) % AES_BLOCK_SIZE;
        for value in 1..=pad_len {
            plaintext.push(value as u8);
        }
        plaintext.push(pad_len as u8);
        plaintext.push(ESP_NEXT_HEADER_IPV4);

        let ciphertext = aes128_cbc_encrypt(&self.outbound_crypt_key, &self.iv, &plaintext)?;
        let mut packet =
            Vec::with_capacity(8 + AES_BLOCK_SIZE + ciphertext.len() + HMAC_MD5_96_LEN);
        packet.extend_from_slice(&self.outbound_spi.to_be_bytes());
        packet.extend_from_slice(&self.sequence.to_be_bytes());
        self.sequence = self.sequence.checked_add(1).unwrap_or(1);
        packet.extend_from_slice(&self.iv);
        packet.extend_from_slice(&ciphertext);
        let icv = hmac_md5_96(&self.outbound_auth_key, &packet)?;
        packet.extend_from_slice(&icv);
        Ok(packet)
    }

    fn decap_ipv4(&self, packet: &[u8]) -> Result<Vec<u8>> {
        if packet.len() < 8 + AES_BLOCK_SIZE + HMAC_MD5_96_LEN {
            bail!("ESP packet is too short");
        }
        let spi = u32::from_be_bytes([packet[0], packet[1], packet[2], packet[3]]);
        if spi != self.inbound_spi {
            bail!(
                "ESP response SPI mismatch: expected 0x{:08x}, got 0x{spi:08x}",
                self.inbound_spi
            );
        }

        let (authenticated, received_icv) = packet.split_at(packet.len() - HMAC_MD5_96_LEN);
        let expected_icv = hmac_md5_96(&self.inbound_auth_key, authenticated)?;
        if !constant_time_eq(received_icv, &expected_icv) {
            bail!("ESP response authentication check failed");
        }

        let ciphertext = &packet[8 + AES_BLOCK_SIZE..packet.len() - HMAC_MD5_96_LEN];
        if ciphertext.is_empty() || ciphertext.len() % AES_BLOCK_SIZE != 0 {
            bail!("ESP ciphertext length is invalid for AES-CBC");
        }
        let iv = &packet[8..8 + AES_BLOCK_SIZE];
        let plaintext = aes128_cbc_decrypt(&self.inbound_crypt_key, iv, ciphertext)?;
        if plaintext.len() < 2 {
            bail!("ESP plaintext is too short");
        }
        let pad_len = plaintext[plaintext.len() - 2] as usize;
        let next_header = plaintext[plaintext.len() - 1];
        if next_header != ESP_NEXT_HEADER_IPV4 {
            bail!("ESP next header is {next_header}, expected IPv4");
        }
        if plaintext.len() < pad_len + 2 {
            bail!("ESP padding length exceeds plaintext length");
        }
        let inner_len = plaintext.len() - pad_len - 2;
        for (index, byte) in plaintext[inner_len..plaintext.len() - 2].iter().enumerate() {
            if *byte != (index + 1) as u8 {
                bail!("ESP padding bytes are invalid");
            }
        }

        Ok(plaintext[..inner_len].to_vec())
    }
}

fn expand_key_material(key_material: &[u8]) -> Vec<u8> {
    let mut expanded = Sha1::digest(key_material).to_vec();
    for _ in 0..9 {
        let digest = Sha1::digest(&expanded);
        expanded.extend_from_slice(&digest);
    }
    expanded
}

fn read_key_slice(bytes: &[u8], offset: &mut usize, size: usize) -> Result<Vec<u8>> {
    let end = *offset + size;
    if end > bytes.len() {
        bail!("expanded Hillstone key material is too short");
    }
    let value = bytes[*offset..end].to_vec();
    *offset = end;
    Ok(value)
}

fn aes128_cbc_encrypt(key: &[u8], iv: &[u8], plaintext: &[u8]) -> Result<Vec<u8>> {
    if plaintext.len() % AES_BLOCK_SIZE != 0 {
        bail!("AES-CBC plaintext length must be block-aligned");
    }
    let cipher = Aes128CbcEnc::new_from_slices(key, iv)
        .map_err(|_| anyhow::anyhow!("invalid AES-128-CBC key or IV length"))?;
    let mut buffer = plaintext.to_vec();
    let plaintext_len = buffer.len();
    let encrypted = cipher
        .encrypt_padded_mut::<NoPadding>(&mut buffer, plaintext_len)
        .map_err(|_| anyhow::anyhow!("failed to AES-CBC encrypt ESP payload"))?;
    Ok(encrypted.to_vec())
}

fn aes128_cbc_decrypt(key: &[u8], iv: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>> {
    if ciphertext.len() % AES_BLOCK_SIZE != 0 {
        bail!("AES-CBC ciphertext length must be block-aligned");
    }
    let cipher = Aes128CbcDec::new_from_slices(key, iv)
        .map_err(|_| anyhow::anyhow!("invalid AES-128-CBC key or IV length"))?;
    let mut buffer = ciphertext.to_vec();
    let decrypted = cipher
        .decrypt_padded_mut::<NoPadding>(&mut buffer)
        .map_err(|_| anyhow::anyhow!("failed to AES-CBC decrypt ESP payload"))?;
    Ok(decrypted.to_vec())
}

fn hmac_md5_96(key: &[u8], data: &[u8]) -> Result<[u8; HMAC_MD5_96_LEN]> {
    let mut mac = <HmacMd5 as Mac>::new_from_slice(key)
        .map_err(|_| anyhow::anyhow!("invalid HMAC-MD5 key length"))?;
    mac.update(data);
    let digest = mac.finalize().into_bytes();
    let mut truncated = [0_u8; HMAC_MD5_96_LEN];
    truncated.copy_from_slice(&digest[..HMAC_MD5_96_LEN]);
    Ok(truncated)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0_u8;
    for (left, right) in left.iter().zip(right) {
        diff |= left ^ right;
    }
    diff == 0
}

fn build_icmp_echo_request(identifier: u16, sequence: u16, payload: &[u8]) -> Vec<u8> {
    let mut packet = Vec::with_capacity(8 + payload.len());
    packet.push(8);
    packet.push(0);
    packet.extend_from_slice(&0_u16.to_be_bytes());
    packet.extend_from_slice(&identifier.to_be_bytes());
    packet.extend_from_slice(&sequence.to_be_bytes());
    packet.extend_from_slice(payload);
    let checksum = internet_checksum(&packet);
    packet[2..4].copy_from_slice(&checksum.to_be_bytes());
    packet
}

fn build_ipv4_packet(
    source: Ipv4Addr,
    destination: Ipv4Addr,
    protocol: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    let total_len = IPV4_HEADER_LEN + payload.len();
    if total_len > u16::MAX as usize {
        bail!("IPv4 packet is too large");
    }

    let mut packet = vec![0_u8; total_len];
    packet[0] = 0x45;
    packet[1] = 0;
    packet[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
    packet[4..6].copy_from_slice(&random_u16().to_be_bytes());
    packet[6..8].copy_from_slice(&0_u16.to_be_bytes());
    packet[8] = 64;
    packet[9] = protocol;
    packet[12..16].copy_from_slice(&source.octets());
    packet[16..20].copy_from_slice(&destination.octets());
    let checksum = internet_checksum(&packet[..IPV4_HEADER_LEN]);
    packet[10..12].copy_from_slice(&checksum.to_be_bytes());
    packet[IPV4_HEADER_LEN..].copy_from_slice(payload);
    Ok(packet)
}

struct TcpSegmentSpec<'a> {
    source_ip: Ipv4Addr,
    destination_ip: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    payload: &'a [u8],
}

fn build_tcp_segment(spec: TcpSegmentSpec<'_>) -> Result<Vec<u8>> {
    let segment_len = TCP_HEADER_LEN + spec.payload.len();
    if segment_len > u16::MAX as usize {
        bail!("TCP segment is too large");
    }

    let mut segment = vec![0_u8; segment_len];
    segment[0..2].copy_from_slice(&spec.source_port.to_be_bytes());
    segment[2..4].copy_from_slice(&spec.destination_port.to_be_bytes());
    segment[4..8].copy_from_slice(&spec.sequence.to_be_bytes());
    segment[8..12].copy_from_slice(&spec.acknowledgement.to_be_bytes());
    segment[12] = (TCP_HEADER_LEN as u8 / 4) << 4;
    segment[13] = spec.flags;
    segment[14..16].copy_from_slice(&64240_u16.to_be_bytes());
    segment[16..18].copy_from_slice(&0_u16.to_be_bytes());
    segment[18..20].copy_from_slice(&0_u16.to_be_bytes());
    segment[TCP_HEADER_LEN..].copy_from_slice(spec.payload);
    let checksum = tcp_checksum(spec.source_ip, spec.destination_ip, &segment)?;
    segment[16..18].copy_from_slice(&checksum.to_be_bytes());
    Ok(segment)
}

fn tcp_checksum(source: Ipv4Addr, destination: Ipv4Addr, segment: &[u8]) -> Result<u16> {
    if segment.len() > u16::MAX as usize {
        bail!("TCP segment is too large");
    }

    let mut checksum_input = Vec::with_capacity(12 + segment.len() + (segment.len() % 2));
    checksum_input.extend_from_slice(&source.octets());
    checksum_input.extend_from_slice(&destination.octets());
    checksum_input.push(0);
    checksum_input.push(IPPROTO_TCP);
    checksum_input.extend_from_slice(&(segment.len() as u16).to_be_bytes());
    checksum_input.extend_from_slice(segment);
    Ok(internet_checksum(&checksum_input))
}

fn parse_tcp_probe_target(value: &str) -> Result<SocketAddrV4> {
    value
        .parse::<SocketAddrV4>()
        .with_context(|| format!("--udp-tcp-probe target must be IPv4:PORT, got {value}"))
}

fn random_ephemeral_port() -> u16 {
    49152 + (random_u16() % 16384)
}

fn tcp_probe_outcome(
    packet: &[u8],
    client_ip: Ipv4Addr,
    target_ip: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    sequence: u32,
) -> Result<TcpProbeOutcome> {
    let parsed = parse_ipv4_tcp_packet(packet)?;
    if parsed.source_ip != target_ip
        || parsed.destination_ip != client_ip
        || parsed.source_port != destination_port
        || parsed.destination_port != source_port
    {
        bail!(
            "UDP ESP TCP probe received unrelated TCP packet: {}",
            describe_ipv4_packet(packet)
        );
    }
    if parsed.flags & TCP_FLAG_SYN != 0
        && parsed.flags & TCP_FLAG_ACK != 0
        && parsed.acknowledgement == sequence.wrapping_add(1)
    {
        return Ok(TcpProbeOutcome::SynAck);
    }
    if parsed.flags & TCP_FLAG_RST != 0 {
        return Ok(TcpProbeOutcome::Reset);
    }
    Ok(TcpProbeOutcome::Other {
        flags: parsed.flags,
        acknowledgement: parsed.acknowledgement,
    })
}

#[derive(Debug, Eq, PartialEq)]
enum TcpProbeOutcome {
    SynAck,
    Reset,
    Other { flags: u8, acknowledgement: u32 },
}

impl TcpProbeOutcome {
    fn is_success(&self) -> bool {
        matches!(self, Self::SynAck)
    }

    fn description(&self) -> String {
        match self {
            Self::SynAck => "received SYN-ACK".to_string(),
            Self::Reset => "received RST".to_string(),
            Self::Other {
                flags,
                acknowledgement,
            } => format!(
                "received unexpected TCP flags={} ack=0x{acknowledgement:08x}",
                describe_tcp_flags(*flags)
            ),
        }
    }
}

struct ParsedTcpPacket {
    source_ip: Ipv4Addr,
    destination_ip: Ipv4Addr,
    source_port: u16,
    destination_port: u16,
    sequence: u32,
    acknowledgement: u32,
    flags: u8,
    payload: Vec<u8>,
}

fn parse_ipv4_tcp_packet(packet: &[u8]) -> Result<ParsedTcpPacket> {
    if packet.len() < IPV4_HEADER_LEN || packet[0] >> 4 != 4 {
        bail!("expected an IPv4 packet");
    }
    let header_len = ((packet[0] & 0x0f) as usize) * 4;
    if header_len < IPV4_HEADER_LEN || packet.len() < header_len + TCP_HEADER_LEN {
        bail!("IPv4 packet is too short for TCP");
    }
    if packet[9] != IPPROTO_TCP {
        bail!("expected TCP over IPv4, got protocol {}", packet[9]);
    }
    let total_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let visible_len = total_len.min(packet.len());
    if visible_len < header_len + TCP_HEADER_LEN {
        bail!("IPv4 total length is too short for TCP");
    }
    let tcp = &packet[header_len..visible_len];
    let tcp_header_len = ((tcp[12] >> 4) as usize) * 4;
    if tcp_header_len < TCP_HEADER_LEN || tcp.len() < tcp_header_len {
        bail!("TCP header length is invalid");
    }

    Ok(ParsedTcpPacket {
        source_ip: Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]),
        destination_ip: Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]),
        source_port: u16::from_be_bytes([tcp[0], tcp[1]]),
        destination_port: u16::from_be_bytes([tcp[2], tcp[3]]),
        sequence: u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]),
        acknowledgement: u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]]),
        flags: tcp[13],
        payload: tcp[tcp_header_len..].to_vec(),
    })
}

struct HttpGetRequest {
    host: Ipv4Addr,
    port: u16,
    path_with_query: String,
}

impl HttpGetRequest {
    fn host_header(&self) -> String {
        if self.port == 80 {
            self.host.to_string()
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }
}

fn parse_http_get_url(value: &str) -> Result<HttpGetRequest> {
    let rest = value
        .strip_prefix("http://")
        .context("--udp-http-get currently supports only http:// URLs")?;
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, format!("/{path}")),
        None => (rest, "/".to_string()),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => {
            let port = port
                .parse::<u16>()
                .with_context(|| format!("invalid URL port in {value}"))?;
            (host, port)
        }
        None => (authority, 80),
    };
    let host = host.parse::<Ipv4Addr>().with_context(|| {
        format!("--udp-http-get currently requires an IPv4 literal host, got {host}")
    })?;
    if path.is_empty() {
        bail!("invalid empty HTTP path");
    }
    Ok(HttpGetRequest {
        host,
        port,
        path_with_query: path,
    })
}

fn http_status_line(response: &[u8]) -> Option<&str> {
    let line_end = response.windows(2).position(|window| window == b"\r\n")?;
    std::str::from_utf8(&response[..line_end]).ok()
}

fn http_response_has_complete_body(response: &[u8]) -> bool {
    let Some(header_end) = find_http_header_end(response) else {
        return false;
    };
    let headers = match std::str::from_utf8(&response[..header_end]) {
        Ok(headers) => headers,
        Err(_) => return false,
    };
    let Some(content_length) = headers.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("content-length") {
            value.trim().parse::<usize>().ok()
        } else {
            None
        }
    }) else {
        return false;
    };
    response.len().saturating_sub(header_end + 4) >= content_length
}

fn find_http_header_end(response: &[u8]) -> Option<usize> {
    response.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_http_request(stream: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>> {
    stream
        .set_read_timeout(Some(timeout))
        .context("failed to set browser request read timeout")?;
    let mut request = Vec::new();
    let mut buffer = [0_u8; 4096];
    let header_end = loop {
        if let Some(header_end) = find_http_header_end(&request) {
            break header_end;
        }
        if request.len() > 64 * 1024 {
            bail!("browser request headers exceeded 64 KiB");
        }
        let size = stream
            .read(&mut buffer)
            .context("failed to read browser request")?;
        if size == 0 {
            bail!("browser closed connection before sending a complete HTTP request");
        }
        request.extend_from_slice(&buffer[..size]);
    };

    let content_length = http_header_content_length(&request[..header_end])?;
    let body_start = header_end + 4;
    while request.len().saturating_sub(body_start) < content_length {
        if request.len() > 10 * 1024 * 1024 {
            bail!("browser request exceeded 10 MiB");
        }
        let size = stream
            .read(&mut buffer)
            .context("failed to read browser request body")?;
        if size == 0 {
            bail!("browser closed connection before sending the full request body");
        }
        request.extend_from_slice(&buffer[..size]);
    }

    Ok(request)
}

fn http_header_content_length(headers: &[u8]) -> Result<usize> {
    let headers = std::str::from_utf8(headers).context("HTTP headers are not valid UTF-8")?;
    for line in headers.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            return value
                .trim()
                .parse::<usize>()
                .context("invalid Content-Length header");
        }
    }
    Ok(0)
}

fn build_upstream_http_request(
    browser_request: &[u8],
    listen: SocketAddrV4,
    target: SocketAddrV4,
) -> Result<UpstreamHttpRequest> {
    let header_end = find_http_header_end(browser_request).context("missing HTTP header end")?;
    let headers = std::str::from_utf8(&browser_request[..header_end])
        .context("browser request headers are not valid UTF-8")?;
    let mut lines = headers.split("\r\n");
    let request_line = lines.next().context("browser request is empty")?;
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts
        .next()
        .context("browser request line is missing method")?;
    let uri = request_parts
        .next()
        .context("browser request line is missing URI")?;
    if method.eq_ignore_ascii_case("CONNECT") {
        bail!("HTTP CONNECT is not supported by the Hillstone browser proxy");
    }
    let path = proxy_request_path(uri)?;

    let mut upstream = Vec::new();
    upstream.extend_from_slice(format!("{method} {path} HTTP/1.1\r\n").as_bytes());
    upstream.extend_from_slice(format!("Host: {}\r\n", host_header_for_socket(target)).as_bytes());
    upstream.extend_from_slice(b"Connection: close\r\n");
    upstream.extend_from_slice(b"Accept-Encoding: identity\r\n");
    let listen_base = format!("http://{}", host_header_for_socket(listen));
    let target_base = format!("http://{}", host_header_for_socket(target));
    let browser_base = browser_base_for_request(headers, listen, target);
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("host")
            || name.eq_ignore_ascii_case("connection")
            || name.eq_ignore_ascii_case("proxy-connection")
            || name.eq_ignore_ascii_case("keep-alive")
            || name.eq_ignore_ascii_case("accept-encoding")
        {
            continue;
        }
        if name.eq_ignore_ascii_case("transfer-encoding") {
            bail!("chunked browser requests are not supported");
        }
        if name.eq_ignore_ascii_case("origin") || name.eq_ignore_ascii_case("referer") {
            // ZenTao's login endpoint returned the HTML login page instead of JSON until these
            // headers matched the internal target. Rewriting Host alone was not enough for its
            // AJAX/proxy checks, because the browser naturally sends the local proxy origin.
            let value = value
                .replace(&browser_base, &target_base)
                .replace(&listen_base, &target_base);
            upstream.extend_from_slice(format!("{name}:{value}").as_bytes());
        } else {
            upstream.extend_from_slice(line.as_bytes());
        }
        upstream.extend_from_slice(b"\r\n");
    }
    upstream.extend_from_slice(b"\r\n");
    upstream.extend_from_slice(&browser_request[header_end + 4..]);
    Ok(UpstreamHttpRequest {
        bytes: upstream,
        method: method.to_string(),
        path,
        browser_base,
    })
}

struct UpstreamHttpRequest {
    bytes: Vec<u8>,
    method: String,
    path: String,
    browser_base: String,
}

fn resolve_http_proxy_target(browser_request: &[u8], listen: SocketAddrV4) -> Result<SocketAddrV4> {
    // Sing-box override_address keeps one system-proxy entry point, but the bridge only sees
    // the rewritten TCP peer (127.0.0.1:16780). The original intranet IP:port must survive in the
    // HTTP request line or Host header; otherwise this bridge cannot know where to send ESP data.
    let header_end = find_http_header_end(browser_request).context("missing HTTP header end")?;
    let headers = std::str::from_utf8(&browser_request[..header_end])
        .context("browser request headers are not valid UTF-8")?;
    let request_line = headers.lines().next().context("browser request is empty")?;
    let uri = request_line
        .split_whitespace()
        .nth(1)
        .context("browser request line is missing URI")?;
    if let Some(target) = http_uri_target(uri, listen) {
        return Ok(target);
    }
    if let Some(target) = request_host_target(headers, listen) {
        return Ok(target);
    }
    bail!("HTTP request does not contain an inferable intranet IPv4 target")
}

fn http_uri_target(uri: &str, listen: SocketAddrV4) -> Option<SocketAddrV4> {
    let rest = uri.strip_prefix("http://")?;
    let authority_end = rest
        .find(|ch| matches!(ch, '/' | '?' | '#'))
        .unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    parse_http_authority_target(authority, listen, 80)
}

fn request_host_target(headers: &str, listen: SocketAddrV4) -> Option<SocketAddrV4> {
    headers.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            parse_http_authority_target(value, listen, 80)
        } else {
            None
        }
    })
}

fn parse_http_authority_target(
    authority: &str,
    listen: SocketAddrV4,
    default_port: u16,
) -> Option<SocketAddrV4> {
    let authority = authority.trim().rsplit('@').next()?.trim();
    let target = authority.parse::<SocketAddrV4>().ok().or_else(|| {
        authority
            .parse::<Ipv4Addr>()
            .ok()
            .map(|ip| SocketAddrV4::new(ip, default_port))
    })?;
    if target == listen { None } else { Some(target) }
}

fn browser_base_for_request(headers: &str, listen: SocketAddrV4, target: SocketAddrV4) -> String {
    let target_host = host_header_for_socket(target);
    let listen_host = host_header_for_socket(listen);
    let request_host = headers.lines().skip(1).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        if name.eq_ignore_ascii_case("host") {
            Some(value.trim())
        } else {
            None
        }
    });
    if request_host
        .map(|host| host.eq_ignore_ascii_case(&target_host))
        .unwrap_or(false)
    {
        // Internal-IP mode uses this same listener as a fixed-target HTTP forward proxy.
        // When the browser Host is already the internal service, response rewrites must keep
        // absolute ZenTao URLs on that internal base instead of converting them back to localhost.
        format!("http://{target_host}")
    } else {
        format!("http://{listen_host}")
    }
}

fn proxy_request_path(uri: &str) -> Result<String> {
    if uri == "*" {
        return Ok(uri.to_string());
    }
    if uri.starts_with('/') {
        return Ok(uri.to_string());
    }
    if let Some(rest) = uri.strip_prefix("http://") {
        let Some(path_start) = rest.find('/') else {
            return Ok("/".to_string());
        };
        return Ok(rest[path_start..].to_string());
    }
    bail!("unsupported browser request URI form: {uri}");
}

fn rewrite_http_response_for_proxy(
    response: &[u8],
    target: SocketAddrV4,
    browser_base: &str,
) -> Vec<u8> {
    let Some(header_end) = find_http_header_end(response) else {
        return response.to_vec();
    };
    let Ok(headers) = std::str::from_utf8(&response[..header_end]) else {
        return response.to_vec();
    };
    let body = &response[header_end + 4..];
    let is_chunked = headers.lines().skip(1).any(|line| {
        let Some((name, value)) = line.split_once(':') else {
            return false;
        };
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    });
    let body = if is_chunked {
        match dechunk_http_body(body) {
            Some(body) => body,
            None => return response.to_vec(),
        }
    } else {
        body.to_vec()
    };

    let target_base = format!("http://{target}");
    let body = replace_bytes(&body, target_base.as_bytes(), browser_base.as_bytes());
    let mut output = Vec::new();
    let mut lines = headers.split("\r\n");
    let Some(status_line) = lines.next() else {
        return response.to_vec();
    };
    output.extend_from_slice(status_line.as_bytes());
    output.extend_from_slice(b"\r\n");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, _)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length")
            || name.eq_ignore_ascii_case("transfer-encoding")
            || name.eq_ignore_ascii_case("connection")
        {
            continue;
        }
        let line = line.replace(&target_base, browser_base);
        output.extend_from_slice(line.as_bytes());
        output.extend_from_slice(b"\r\n");
    }
    output.extend_from_slice(b"Connection: close\r\n");
    output.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    output.extend_from_slice(b"\r\n");
    output.extend_from_slice(&body);
    output
}

fn dechunk_http_body(body: &[u8]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    let mut index = 0;
    loop {
        let line_end = find_crlf(&body[index..])? + index;
        let line = std::str::from_utf8(&body[index..line_end]).ok()?;
        let size = usize::from_str_radix(line.split(';').next()?.trim(), 16).ok()?;
        index = line_end + 2;
        if size == 0 {
            return Some(output);
        }
        if body.len() < index + size + 2 {
            return None;
        }
        output.extend_from_slice(&body[index..index + size]);
        index += size;
        if body.get(index..index + 2)? != b"\r\n" {
            return None;
        }
        index += 2;
    }
}

fn find_crlf(bytes: &[u8]) -> Option<usize> {
    bytes.windows(2).position(|window| window == b"\r\n")
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return haystack.to_vec();
    }
    let mut output = Vec::with_capacity(haystack.len());
    let mut index = 0;
    while let Some(relative) = haystack[index..]
        .windows(needle.len())
        .position(|window| window == needle)
    {
        let absolute = index + relative;
        output.extend_from_slice(&haystack[index..absolute]);
        output.extend_from_slice(replacement);
        index = absolute + needle.len();
    }
    output.extend_from_slice(&haystack[index..]);
    output
}

fn host_header_for_socket(addr: SocketAddrV4) -> String {
    if addr.port() == 80 {
        addr.ip().to_string()
    } else {
        addr.to_string()
    }
}

fn write_simple_http_error(stream: &mut TcpStream, status: u16, reason: &str) -> io::Result<()> {
    let body = format!("{status} {reason}\n");
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{body}",
        body.len()
    )
}

fn describe_ipv4_packet(packet: &[u8]) -> String {
    if packet.len() < IPV4_HEADER_LEN || packet[0] >> 4 != 4 {
        return format!("invalid IPv4 packet len={}", packet.len());
    }
    let header_len = ((packet[0] & 0x0f) as usize) * 4;
    if header_len < IPV4_HEADER_LEN || packet.len() < header_len {
        return format!(
            "invalid IPv4 header len={header_len} packet_len={}",
            packet.len()
        );
    }
    let total_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let visible_len = total_len.min(packet.len());
    let protocol = packet[9];
    let source = Ipv4Addr::new(packet[12], packet[13], packet[14], packet[15]);
    let destination = Ipv4Addr::new(packet[16], packet[17], packet[18], packet[19]);
    let mut description = format!("{source} -> {destination} proto={protocol} len={visible_len}");
    if protocol == IPPROTO_ICMP && visible_len >= header_len + 8 {
        let icmp = &packet[header_len..visible_len];
        let icmp_type = icmp[0];
        let icmp_code = icmp[1];
        let identifier = u16::from_be_bytes([icmp[4], icmp[5]]);
        let sequence = u16::from_be_bytes([icmp[6], icmp[7]]);
        description.push_str(&format!(
            " icmp_type={icmp_type} icmp_code={icmp_code} id=0x{identifier:04x} seq={sequence}"
        ));
    } else if protocol == IPPROTO_TCP && visible_len >= header_len + TCP_HEADER_LEN {
        let tcp = &packet[header_len..visible_len];
        let source_port = u16::from_be_bytes([tcp[0], tcp[1]]);
        let destination_port = u16::from_be_bytes([tcp[2], tcp[3]]);
        let sequence = u32::from_be_bytes([tcp[4], tcp[5], tcp[6], tcp[7]]);
        let acknowledgement = u32::from_be_bytes([tcp[8], tcp[9], tcp[10], tcp[11]]);
        let flags = tcp[13];
        description.push_str(&format!(
            " tcp={source_port}->{destination_port} flags={} seq=0x{sequence:08x} ack=0x{acknowledgement:08x}",
            describe_tcp_flags(flags)
        ));
    }
    description
}

fn describe_tcp_flags(flags: u8) -> String {
    let mut names = Vec::new();
    if flags & TCP_FLAG_FIN != 0 {
        names.push("FIN");
    }
    if flags & TCP_FLAG_SYN != 0 {
        names.push("SYN");
    }
    if flags & TCP_FLAG_RST != 0 {
        names.push("RST");
    }
    if flags & TCP_FLAG_PSH != 0 {
        names.push("PSH");
    }
    if flags & TCP_FLAG_ACK != 0 {
        names.push("ACK");
    }
    if names.is_empty() {
        format!("0x{flags:02x}")
    } else {
        names.join("|")
    }
}

fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum = 0_u32;
    let mut chunks = bytes.chunks_exact(2);
    for chunk in &mut chunks {
        sum += u16::from_be_bytes([chunk[0], chunk[1]]) as u32;
    }
    if let [byte] = chunks.remainder() {
        sum += u16::from_be_bytes([*byte, 0]) as u32;
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn send_message(stream: &mut TlsStream<TcpStream>, message: Message) -> Result<()> {
    stream
        .write_all(&message.finish())
        .context("failed to write Hillstone message")
}

fn read_non_empty_frame(stream: &mut TlsStream<TcpStream>) -> Result<Frame> {
    loop {
        let frame = read_frame(stream)?;
        if frame.message_type != MessageType::None {
            return Ok(frame);
        }
    }
}

fn read_frame(stream: &mut TlsStream<TcpStream>) -> Result<Frame> {
    let mut header = [0_u8; 8];
    stream
        .read_exact(&mut header)
        .context("failed to read Hillstone frame header")?;
    if header == [0; 8] {
        return Ok(Frame {
            message_type: MessageType::None,
            payloads: BTreeMap::new(),
            reply: true,
        });
    }

    let magic = header[0];
    if magic != 0x22 {
        bail!("invalid Hillstone frame magic: 0x{magic:02x}");
    }
    let reply = header[1] == 0x02;
    let message_type = MessageType::from_u16(u16::from_be_bytes([header[2], header[3]]));
    let frame_size = u32::from_be_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if frame_size < 8 {
        bail!("invalid Hillstone frame size: {frame_size}");
    }
    let mut data = vec![0_u8; frame_size - 8];
    stream
        .read_exact(&mut data)
        .context("failed to read Hillstone frame payload")?;

    let mut payloads = BTreeMap::new();
    let mut cursor = 0;
    while cursor < data.len() {
        if data.len() - cursor < 4 {
            bail!("truncated Hillstone TLV header");
        }
        let key = u16::from_be_bytes([data[cursor], data[cursor + 1]]);
        let len = u16::from_be_bytes([data[cursor + 2], data[cursor + 3]]) as usize;
        cursor += 4;
        if data.len() - cursor < len {
            bail!("truncated Hillstone TLV value for payload {key}");
        }
        payloads.insert(key, data[cursor..cursor + len].to_vec());
        cursor += len;
        cursor += (4 - (cursor % 4)) % 4;
    }

    Ok(Frame {
        message_type,
        payloads,
        reply,
    })
}

fn print_frame_summary(label: &str, frame: &Frame) {
    let payload_names = frame
        .payloads
        .keys()
        .map(|key| payload_name(*key).to_string())
        .collect::<Vec<_>>()
        .join(", ");
    eprintln!(
        "{label}: type={} reply={} payloads=[{}]",
        frame.message_type.name(),
        frame.reply,
        payload_names
    );
}

fn ensure_ok_status(frame: &Frame, context: &str) -> Result<()> {
    let Some(status) = frame.payload_u32(Payload::Status) else {
        return Ok(());
    };
    if status == 0 {
        return Ok(());
    }
    bail!(
        "{context} returned status {status} ({})",
        status_name(status).unwrap_or("unknown")
    );
}

#[derive(Default)]
struct NetworkSetup {
    server_udp_port: Option<u16>,
    client_private_ipv4: Option<Ipv4Addr>,
    server_private_ipv4: Option<Ipv4Addr>,
    prefix_len: Option<u32>,
    dns_ipv4: Vec<Ipv4Addr>,
    route_ipv4: Vec<u8>,
}

struct NewKeySummary {
    enc_alg: Option<u16>,
    auth_alg: Option<u16>,
    ipcomp_alg: Option<u16>,
    outbound_spi: Option<u32>,
    inbound_spi: u32,
    key_material: [u8; 0x30],
    session_id_present: bool,
}

struct Frame {
    message_type: MessageType,
    payloads: BTreeMap<u16, Vec<u8>>,
    reply: bool,
}

impl Frame {
    fn payload_u16(&self, payload: Payload) -> Option<u16> {
        let bytes = self.payloads.get(&(payload as u16))?;
        if bytes.len() != 2 {
            return None;
        }
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn payload_u32(&self, payload: Payload) -> Option<u32> {
        let bytes = self.payloads.get(&(payload as u16))?;
        if bytes.len() != 4 {
            return None;
        }
        Some(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn payload_ipv4(&self, payload: Payload) -> Option<Ipv4Addr> {
        let bytes = self.payloads.get(&(payload as u16))?;
        if bytes.len() != 4 {
            return None;
        }
        Some(Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3]))
    }
}

struct Message {
    message_type: MessageType,
    data: Vec<(Payload, Vec<u8>)>,
}

impl Message {
    fn new(message_type: MessageType) -> Self {
        Self {
            message_type,
            data: Vec::new(),
        }
    }

    fn push_u16(&mut self, payload: Payload, value: u16) {
        self.data.push((payload, value.to_be_bytes().to_vec()));
    }

    fn push_u32(&mut self, payload: Payload, value: u32) {
        self.data.push((payload, value.to_be_bytes().to_vec()));
    }

    fn push_ipv4(&mut self, payload: Payload, value: Ipv4Addr) {
        self.data.push((payload, value.octets().to_vec()));
    }

    fn push_str(&mut self, payload: Payload, value: &str) {
        self.push_bytes(payload, value.as_bytes());
    }

    fn push_bytes(&mut self, payload: Payload, value: &[u8]) {
        self.data.push((payload, value.to_vec()));
    }

    fn finish(self) -> Vec<u8> {
        let mut body = Vec::new();
        for (payload, value) in self.data {
            body.extend_from_slice(&(payload as u16).to_be_bytes());
            body.extend_from_slice(&(value.len() as u16).to_be_bytes());
            body.extend_from_slice(&value);
            let padding = (4 - (body.len() % 4)) % 4;
            body.resize(body.len() + padding, 0);
        }

        let mut packet = Vec::with_capacity(body.len() + 8);
        packet.push(0x22);
        packet.push(0x00);
        packet.extend_from_slice(&(self.message_type as u16).to_be_bytes());
        packet.extend_from_slice(&((body.len() + 8) as u32).to_be_bytes());
        packet.extend_from_slice(&body);
        packet
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
enum MessageType {
    None = 0x00,
    Auth = 0x01,
    ClientInfo = 0x02,
    SetIp = 0x03,
    SetRoute = 0x04,
    NewKey = 0x05,
    KeyDone = 0x06,
    ClientLogout = 0x07,
    ServerDisconnect = 0x08,
    KeepAlive = 0x09,
    Rekey = 0x0a,
    HostCheck = 0x0b,
    HostCheckUpdate = 0x0c,
    ChangePassword = 0x0d,
    ChangePasswordResponse = 0x0e,
    SmsAuthRequest = 0x0f,
    SmsAuthRequestResponse = 0x10,
    SmsRequest = 0x11,
    SmsRequestResponse = 0x12,
    RsaNewPin = 0x13,
    RsaNewPinResponse = 0x14,
    Unknown = 0xffff,
}

impl MessageType {
    fn from_u16(value: u16) -> Self {
        match value {
            0x00 => Self::None,
            0x01 => Self::Auth,
            0x02 => Self::ClientInfo,
            0x03 => Self::SetIp,
            0x04 => Self::SetRoute,
            0x05 => Self::NewKey,
            0x06 => Self::KeyDone,
            0x07 => Self::ClientLogout,
            0x08 => Self::ServerDisconnect,
            0x09 => Self::KeepAlive,
            0x0a => Self::Rekey,
            0x0b => Self::HostCheck,
            0x0c => Self::HostCheckUpdate,
            0x0d => Self::ChangePassword,
            0x0e => Self::ChangePasswordResponse,
            0x0f => Self::SmsAuthRequest,
            0x10 => Self::SmsAuthRequestResponse,
            0x11 => Self::SmsRequest,
            0x12 => Self::SmsRequestResponse,
            0x13 => Self::RsaNewPin,
            0x14 => Self::RsaNewPinResponse,
            _ => Self::Unknown,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::None => "NONE",
            Self::Auth => "AUTH",
            Self::ClientInfo => "CLNT_INFO",
            Self::SetIp => "SET_IP",
            Self::SetRoute => "SET_ROUTE",
            Self::NewKey => "NEW_KEY",
            Self::KeyDone => "KEY_DONE",
            Self::ClientLogout => "CLNT_LOGOUT",
            Self::ServerDisconnect => "SERV_DISCONN",
            Self::KeepAlive => "KEEP_ALIVE",
            Self::Rekey => "REKEY",
            Self::HostCheck => "HOST_CHECK",
            Self::HostCheckUpdate => "HOST_CHECK_UPD",
            Self::ChangePassword => "CHPWD",
            Self::ChangePasswordResponse => "CHPWD_RESP",
            Self::SmsAuthRequest => "SMS_AUTH_REQ",
            Self::SmsAuthRequestResponse => "SMS_AUTH_REQ_RSP",
            Self::SmsRequest => "SMS_REQ",
            Self::SmsRequestResponse => "SMS_REQ_RSP",
            Self::RsaNewPin => "RSA_NEWPIN",
            Self::RsaNewPinResponse => "RSA_NEWPIN_RSP",
            Self::Unknown => "UNKNOWN",
        }
    }
}

#[derive(Clone, Copy)]
#[repr(u16)]
enum Payload {
    Username = 1,
    Password = 2,
    Status = 4,
    ClientPublicIpv4 = 5,
    ServerPublicIpv4 = 6,
    ClientPrivateIpv4 = 7,
    ServerPrivateIpv4 = 8,
    ServerUdpPort = 9,
    NetmaskIpv4 = 11,
    Keymat = 16,
    DnsIpv4 = 17,
    RouteIpv4 = 19,
    KeyExchangeMode = 32,
    Spi = 48,
    EncAlg = 49,
    AuthAlg = 50,
    SessionId = 51,
    AuthType = 81,
    Disconnect = 83,
    ClientVer = 84,
    HostId = 96,
    HostName = 97,
    IpcompCpi = 128,
    IpcompAlg = 129,
}

fn payload_name(value: u16) -> &'static str {
    match value {
        1 => "USERNAME",
        2 => "PASSWORD",
        3 => "CHAL_PASSWORD",
        4 => "STATUS",
        5 => "CLT_PUB_IPV4",
        6 => "SVR_PUB_IPV4",
        7 => "CLT_PRIV_IPV4",
        8 => "SVR_PRIV_IPV4",
        9 => "SVR_UDP_PORT",
        10 => "IP_SUBNET",
        11 => "NETMASK_IPV4",
        12 => "GATEWAY_IPV4",
        13 => "ROUTE_METRICS",
        14 => "IPSEC_SETTING",
        15 => "MODP_GROUP",
        16 => "KEYMAT",
        17 => "DNS_IPV4",
        18 => "WINS_IPV4",
        19 => "ROUTE_IPV4",
        20 => "PERFE_SRV_IPV4",
        21 => "COMM_SRV_IPV4",
        32 => "KEY_EXCH_MODE",
        48 => "SPI",
        49 => "ENC_ALG",
        50 => "AUTH_ALG",
        51 => "SESSION_ID",
        53 => "EN_ERRO_MSG",
        54 => "CH_ERRO_MSG",
        64 => "ALIVE_STATUS",
        81 => "AUTH_TYPE",
        82 => "COOKIE",
        83 => "DISCONNECT",
        84 => "CLIENT_VER",
        96 => "HOST_ID",
        97 => "HOST_NAME",
        112 => "HOST_CHECK_MD5",
        115 => "HOST_CHECK_RESULT",
        116 => "HOST_CHECK_RESULT_SIZE",
        128 => "IPCOMP_CPI",
        129 => "IPCOMP_ALG",
        132 => "ALLOW_PWD",
        133 => "NEED_SMS_AUTH",
        134 => "SMS_AUTH_CODE",
        136 => "CLIENT_AUTO_CONNECT",
        _ => "UNKNOWN",
    }
}

fn read_password(options: &HillstoneProbeOptions) -> Result<String> {
    if let Some(password) = options.password.as_ref().filter(|value| !value.is_empty()) {
        return Ok(password.clone());
    }

    if options.password_stdin {
        let mut password = String::new();
        io::stdin()
            .read_line(&mut password)
            .context("failed to read password from stdin")?;
        return Ok(password.trim_end_matches(['\r', '\n']).to_string());
    }

    let env_name = options
        .password_env
        .as_deref()
        .unwrap_or("HILLSTONE_PASSWORD");
    env::var(env_name).with_context(|| {
        format!("set {env_name} or pass --password-stdin to provide the Hillstone password")
    })
}

fn load_hillstone_uuid() -> Option<String> {
    let home = env::var_os("HOME")?;
    let path = PathBuf::from(home)
        .join("Library")
        .join("Preferences")
        .join("HillstoneSecureConnect")
        .join("AppConfig.ini");
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("UUID=").map(str::trim))
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn local_host_name() -> Option<String> {
    env::var("HOSTNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|output| {
                    if output.status.success() {
                        String::from_utf8(output.stdout).ok()
                    } else {
                        None
                    }
                })
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        })
}

fn resolve_ipv4(host: &str) -> Result<Ipv4Addr> {
    (host, 0)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {host}"))?
        .find_map(|addr| match addr.ip() {
            std::net::IpAddr::V4(ip) => Some(ip),
            std::net::IpAddr::V6(_) => None,
        })
        .with_context(|| format!("{host} did not resolve to an IPv4 address"))
}

fn local_ipv4_for_remote(host: &str, port: u16) -> Result<Ipv4Addr> {
    let remote = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {host}:{port}"))?
        .find(|addr| addr.is_ipv4())
        .with_context(|| format!("{host}:{port} did not resolve to an IPv4 address"))?;
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))
        .context("failed to bind UDP socket for local IPv4 detection")?;
    socket
        .connect(remote)
        .context("failed to determine local IPv4 route to Hillstone gateway")?;
    match socket
        .local_addr()
        .context("failed to read local UDP socket address")?
        .ip()
    {
        std::net::IpAddr::V4(ip) => Ok(ip),
        std::net::IpAddr::V6(_) => bail!("local route to Hillstone gateway selected IPv6"),
    }
}

fn decode_ipv4_list(bytes: &[u8]) -> Vec<Ipv4Addr> {
    bytes
        .chunks_exact(4)
        .map(|chunk| Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]))
        .collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Ipv4RouteEntry {
    destination: Ipv4Addr,
    prefix_len: u32,
    metric: Option<u32>,
}

impl Ipv4RouteEntry {
    fn cidr(&self) -> String {
        format!("{}/{}", self.destination, self.prefix_len)
    }
}

impl std::fmt::Display for Ipv4RouteEntry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(metric) = self.metric {
            write!(
                formatter,
                "{}/{} metric {metric}",
                self.destination, self.prefix_len
            )
        } else {
            write!(formatter, "{}/{}", self.destination, self.prefix_len)
        }
    }
}

fn decode_ipv4_route_entries(bytes: &[u8]) -> Option<Vec<Ipv4RouteEntry>> {
    if bytes.len() % 12 == 0 {
        return Some(
            bytes
                .chunks_exact(12)
                .map(|chunk| {
                    let destination = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let mask = Ipv4Addr::new(chunk[4], chunk[5], chunk[6], chunk[7]);
                    let metric = u32::from_be_bytes([chunk[8], chunk[9], chunk[10], chunk[11]]);
                    Ipv4RouteEntry {
                        destination,
                        prefix_len: netmask_prefix_len(mask),
                        metric: Some(metric),
                    }
                })
                .collect(),
        );
    }
    if bytes.len() % 8 == 0 {
        return Some(
            bytes
                .chunks_exact(8)
                .map(|chunk| {
                    let destination = Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
                    let mask = Ipv4Addr::new(chunk[4], chunk[5], chunk[6], chunk[7]);
                    Ipv4RouteEntry {
                        destination,
                        prefix_len: netmask_prefix_len(mask),
                        metric: None,
                    }
                })
                .collect(),
        );
    }
    None
}

fn decode_ipv4_route_cidrs(bytes: &[u8]) -> Vec<String> {
    decode_ipv4_route_entries(bytes)
        .unwrap_or_default()
        .into_iter()
        .map(|route| route.cidr())
        .collect()
}

fn decode_ipv4_routes(bytes: &[u8]) -> Vec<String> {
    if let Some(routes) = decode_ipv4_route_entries(bytes) {
        return routes.into_iter().map(|route| route.to_string()).collect();
    }
    vec![format!("unparsed hex {}", short_hex(bytes))]
}

fn netmask_prefix_len(mask: Ipv4Addr) -> u32 {
    u32::from(mask).count_ones()
}

fn join_ipv4(values: &[Ipv4Addr]) -> String {
    values
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn fill_random(bytes: &mut [u8]) {
    for byte in bytes {
        *byte = rand::random();
    }
}

fn random_u32() -> u32 {
    rand::random()
}

fn random_u16() -> u16 {
    rand::random()
}

fn describe_algorithm(value: Option<u16>, name: fn(u16) -> &'static str) -> String {
    match value {
        Some(value) => format!("{value} ({})", name(value)),
        None => "missing".to_string(),
    }
}

fn encryption_algorithm_name(value: u16) -> &'static str {
    match value {
        0 => "null",
        1 => "des-cbc",
        3 => "3des-cbc",
        12 => "aes128-cbc",
        14 => "aes192-cbc",
        15 => "aes256-cbc",
        _ => "unknown",
    }
}

fn auth_algorithm_name(value: u16) -> &'static str {
    match value {
        0 => "hmac-null",
        1 => "hmac-md5-96",
        2 => "hmac-sha1-96",
        5 => "hmac-sha256-128",
        6 => "hmac-sha384-192",
        7 => "hmac-sha512-256",
        _ => "unknown",
    }
}

fn ipcomp_algorithm_name(value: u16) -> &'static str {
    match value {
        0 => "none",
        2 => "deflate",
        _ => "unknown",
    }
}

fn status_name(value: u32) -> Option<&'static str> {
    match value {
        1 | 3 => Some("wrong_username_password"),
        5 => Some("require_certificate"),
        6 => Some("wrong_hardware_id"),
        16 => Some("require_sms"),
        21 => Some("wrong_phone_number"),
        _ => None,
    }
}

fn short_hex(bytes: &[u8]) -> String {
    const LIMIT: usize = 48;
    let mut out = bytes
        .iter()
        .take(LIMIT)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join("");
    if bytes.len() > LIMIT {
        out.push_str("...");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::net::{Ipv4Addr, SocketAddrV4};

    use super::{
        AES_BLOCK_SIZE, EspSession, IPPROTO_ICMP, IPPROTO_TCP, TCP_FLAG_ACK, TCP_FLAG_SYN,
        TcpProbeOutcome, TcpSegmentSpec, build_icmp_echo_request, build_ipv4_packet,
        build_tcp_segment, build_upstream_http_request, decode_ipv4_route_cidrs,
        decode_ipv4_routes, describe_ipv4_packet, http_response_has_complete_body,
        http_status_line, internet_checksum, parse_http_get_url, resolve_http_proxy_target,
        rewrite_http_response_for_proxy, tcp_probe_outcome,
    };

    #[test]
    fn decodes_twelve_byte_ipv4_routes_with_metrics() {
        let bytes = [
            10, 1, 0, 0, 255, 255, 0, 0, 0, 0, 0, 35, 10, 255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 35,
            10, 253, 0, 0, 255, 255, 255, 0, 0, 0, 0, 35,
        ];

        assert_eq!(
            decode_ipv4_routes(&bytes),
            [
                "10.1.0.0/16 metric 35",
                "10.255.0.0/24 metric 35",
                "10.253.0.0/24 metric 35"
            ]
        );
    }

    #[test]
    fn decodes_ipv4_route_cidrs_for_config_application() {
        let bytes = [
            10, 1, 0, 0, 255, 255, 0, 0, 0, 0, 0, 35, 10, 255, 0, 0, 255, 255, 255, 0, 0, 0, 0, 35,
            10, 253, 0, 0, 255, 255, 255, 0, 0, 0, 0, 35,
        ];

        assert_eq!(
            decode_ipv4_route_cidrs(&bytes),
            ["10.1.0.0/16", "10.255.0.0/24", "10.253.0.0/24"]
        );
    }

    #[test]
    fn esp_aes_md5_round_trips_inner_ipv4_packet() {
        let mut session = test_esp_session();
        let icmp = build_icmp_echo_request(0x1234, 7, b"payload");
        let inner = build_ipv4_packet(
            Ipv4Addr::new(10, 250, 252, 93),
            Ipv4Addr::new(10, 250, 252, 1),
            IPPROTO_ICMP,
            &icmp,
        )
        .expect("build IPv4 packet");

        let esp = session.encap_ipv4(&inner).expect("encap ESP packet");

        assert_eq!(&esp[..4], &0x1020_3040_u32.to_be_bytes());
        assert_eq!(&esp[8..8 + AES_BLOCK_SIZE], &[0x33; AES_BLOCK_SIZE]);
        assert_eq!(session.sequence, 2);
        assert_eq!(session.decap_ipv4(&esp).expect("decap ESP packet"), inner);
    }

    #[test]
    fn esp_rejects_tampered_authentication_data() {
        let mut session = test_esp_session();
        let icmp = build_icmp_echo_request(0x1234, 7, b"payload");
        let inner = build_ipv4_packet(
            Ipv4Addr::new(10, 250, 252, 93),
            Ipv4Addr::new(10, 250, 252, 1),
            IPPROTO_ICMP,
            &icmp,
        )
        .expect("build IPv4 packet");
        let mut esp = session.encap_ipv4(&inner).expect("encap ESP packet");

        let last = esp.len() - 1;
        esp[last] ^= 0xff;

        let error = session
            .decap_ipv4(&esp)
            .expect_err("tampered packet should fail authentication");
        assert!(error.to_string().contains("authentication check failed"));
    }

    #[test]
    fn builds_ipv4_icmp_echo_with_valid_checksums() {
        let icmp = build_icmp_echo_request(0x1234, 1, b"abc");
        assert_eq!(icmp[0], 8);
        assert_eq!(icmp[1], 0);
        assert_eq!(internet_checksum(&icmp), 0);

        let inner = build_ipv4_packet(
            Ipv4Addr::new(10, 250, 252, 93),
            Ipv4Addr::new(10, 250, 252, 1),
            IPPROTO_ICMP,
            &icmp,
        )
        .expect("build IPv4 packet");
        assert_eq!(inner[0], 0x45);
        assert_eq!(inner[9], IPPROTO_ICMP);
        assert_eq!(internet_checksum(&inner[..20]), 0);
        assert_eq!(
            describe_ipv4_packet(&inner),
            "10.250.252.93 -> 10.250.252.1 proto=1 len=31 icmp_type=8 icmp_code=0 id=0x1234 seq=1"
        );
    }

    #[test]
    fn builds_tcp_syn_with_valid_checksum_and_summary() {
        let source_ip = Ipv4Addr::new(10, 250, 252, 77);
        let destination_ip = Ipv4Addr::new(10, 1, 126, 5);
        let tcp = build_tcp_segment(TcpSegmentSpec {
            source_ip,
            destination_ip,
            source_port: 50123,
            destination_port: 10011,
            sequence: 0x1122_3344,
            acknowledgement: 0,
            flags: TCP_FLAG_SYN,
            payload: &[],
        })
        .expect("build TCP SYN");
        assert_eq!(tcp[0..2], 50123_u16.to_be_bytes());
        assert_eq!(tcp[2..4], 10011_u16.to_be_bytes());
        assert_eq!(tcp[12], 0x50);
        assert_eq!(tcp[13], TCP_FLAG_SYN);

        let inner =
            build_ipv4_packet(source_ip, destination_ip, IPPROTO_TCP, &tcp).expect("build IPv4");
        assert_eq!(
            describe_ipv4_packet(&inner),
            "10.250.252.77 -> 10.1.126.5 proto=6 len=40 tcp=50123->10011 flags=SYN seq=0x11223344 ack=0x00000000"
        );
    }

    #[test]
    fn tcp_probe_outcome_accepts_matching_syn_ack() {
        let client_ip = Ipv4Addr::new(10, 250, 252, 77);
        let target_ip = Ipv4Addr::new(10, 1, 126, 5);
        let response = build_tcp_segment(TcpSegmentSpec {
            source_ip: target_ip,
            destination_ip: client_ip,
            source_port: 10011,
            destination_port: 50123,
            sequence: 0xaabb_ccdd,
            acknowledgement: 0x1122_3345,
            flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
            payload: &[],
        })
        .expect("build TCP SYN-ACK");
        let inner =
            build_ipv4_packet(target_ip, client_ip, IPPROTO_TCP, &response).expect("build IPv4");

        assert_eq!(
            tcp_probe_outcome(&inner, client_ip, target_ip, 50123, 10011, 0x1122_3344)
                .expect("parse TCP outcome"),
            TcpProbeOutcome::SynAck
        );
    }

    #[test]
    fn parses_ipv4_http_get_url() {
        let request = parse_http_get_url("http://10.1.126.5:10011/bug-browse.html?x=1")
            .expect("parse HTTP URL");

        assert_eq!(request.host, Ipv4Addr::new(10, 1, 126, 5));
        assert_eq!(request.port, 10011);
        assert_eq!(request.path_with_query, "/bug-browse.html?x=1");
        assert_eq!(request.host_header(), "10.1.126.5:10011");
    }

    #[test]
    fn detects_http_status_and_complete_content_length_body() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";

        assert_eq!(http_status_line(response), Some("HTTP/1.1 200 OK"));
        assert!(http_response_has_complete_body(response));
        assert!(!http_response_has_complete_body(
            b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\n\r\nhello"
        ));
    }

    #[test]
    fn builds_upstream_proxy_request_for_internal_target() {
        let browser_request = b"GET /bug-browse.html?x=1 HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nUser-Agent: test\r\nAccept-Encoding: gzip\r\nConnection: keep-alive\r\nOrigin: http://127.0.0.1:18080\r\nReferer: http://127.0.0.1:18080/user-login.html\r\n\r\n";
        let proxy_request = build_upstream_http_request(
            browser_request,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 10011),
        )
        .expect("build upstream request");
        let upstream = String::from_utf8(proxy_request.bytes).expect("upstream request is UTF-8");

        assert_eq!(proxy_request.method, "GET");
        assert_eq!(proxy_request.path, "/bug-browse.html?x=1");
        assert_eq!(proxy_request.browser_base, "http://127.0.0.1:18080");
        assert!(upstream.starts_with("GET /bug-browse.html?x=1 HTTP/1.1\r\n"));
        assert!(upstream.contains("\r\nHost: 10.1.126.5:10011\r\n"));
        assert!(upstream.contains("\r\nAccept-Encoding: identity\r\n"));
        assert!(upstream.contains("\r\nOrigin: http://10.1.126.5:10011\r\n"));
        assert!(upstream.contains("\r\nReferer: http://10.1.126.5:10011/user-login.html\r\n"));
        assert!(!upstream.contains("127.0.0.1:18080"));
        assert!(!upstream.contains("keep-alive"));
    }

    #[test]
    fn builds_forward_proxy_request_with_internal_browser_base() {
        let browser_request = b"GET http://10.1.126.5:10011/bug-view-1.html HTTP/1.1\r\nHost: 10.1.126.5:10011\r\nUser-Agent: test\r\nProxy-Connection: keep-alive\r\nOrigin: http://10.1.126.5:10011\r\nReferer: http://10.1.126.5:10011/bug-browse.html\r\n\r\n";
        let proxy_request = build_upstream_http_request(
            browser_request,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 10011),
        )
        .expect("build upstream request");
        let upstream = String::from_utf8(proxy_request.bytes).expect("upstream request is UTF-8");

        assert_eq!(proxy_request.path, "/bug-view-1.html");
        assert_eq!(proxy_request.browser_base, "http://10.1.126.5:10011");
        assert!(upstream.starts_with("GET /bug-view-1.html HTTP/1.1\r\n"));
        assert!(upstream.contains("\r\nHost: 10.1.126.5:10011\r\n"));
        assert!(upstream.contains("\r\nOrigin: http://10.1.126.5:10011\r\n"));
        assert!(upstream.contains("\r\nReferer: http://10.1.126.5:10011/bug-browse.html\r\n"));
        assert!(!upstream.contains("127.0.0.1:18080"));
        assert!(!upstream.contains("Proxy-Connection"));
    }

    #[test]
    fn resolves_proxy_target_from_internal_host_header() {
        let browser_request =
            b"GET / HTTP/1.1\r\nHost: 10.1.126.5:8099\r\nUser-Agent: test\r\n\r\n";

        let target = resolve_http_proxy_target(
            browser_request,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
        )
        .expect("target resolves");

        assert_eq!(
            target,
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 8099)
        );
    }

    #[test]
    fn resolves_proxy_target_from_absolute_uri() {
        let browser_request = b"GET http://10.1.126.5:8099/index.html HTTP/1.1\r\nHost: 10.1.126.5:10011\r\nUser-Agent: test\r\n\r\n";

        let target = resolve_http_proxy_target(
            browser_request,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
        )
        .expect("target resolves");

        assert_eq!(
            target,
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 8099)
        );
    }

    #[test]
    fn rejects_proxy_target_when_request_only_names_local_listener() {
        let browser_request =
            b"GET / HTTP/1.1\r\nHost: 127.0.0.1:18080\r\nUser-Agent: test\r\n\r\n";

        let error = resolve_http_proxy_target(
            browser_request,
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 18080),
        )
        .expect_err("local listener request has no intranet target");

        assert!(
            error
                .to_string()
                .contains("does not contain an inferable intranet IPv4 target")
        );
    }

    #[test]
    fn rewrites_proxy_response_absolute_internal_urls() {
        let response = b"HTTP/1.1 302 Found\r\nLocation: http://10.1.126.5:10011/next\r\nContent-Length: 41\r\nConnection: keep-alive\r\n\r\n<a href=\"http://10.1.126.5:10011/next\">";
        let rewritten = rewrite_http_response_for_proxy(
            response,
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 10011),
            "http://127.0.0.1:18080",
        );
        let rewritten = String::from_utf8(rewritten).expect("rewritten response is UTF-8");

        assert!(rewritten.contains("Location: http://127.0.0.1:18080/next"));
        assert!(rewritten.contains("<a href=\"http://127.0.0.1:18080/next\">"));
        assert!(rewritten.contains("Connection: close"));
        assert!(!rewritten.contains("10.1.126.5:10011"));
    }

    #[test]
    fn keeps_proxy_response_internal_urls_for_forward_proxy_mode() {
        let response = b"HTTP/1.1 302 Found\r\nLocation: http://10.1.126.5:10011/next\r\nContent-Length: 41\r\nConnection: keep-alive\r\n\r\n<a href=\"http://10.1.126.5:10011/next\">";
        let rewritten = rewrite_http_response_for_proxy(
            response,
            SocketAddrV4::new(Ipv4Addr::new(10, 1, 126, 5), 10011),
            "http://10.1.126.5:10011",
        );
        let rewritten = String::from_utf8(rewritten).expect("rewritten response is UTF-8");

        assert!(rewritten.contains("Location: http://10.1.126.5:10011/next"));
        assert!(rewritten.contains("<a href=\"http://10.1.126.5:10011/next\">"));
        assert!(rewritten.contains("Connection: close"));
        assert!(!rewritten.contains("127.0.0.1:18080"));
    }

    fn test_esp_session() -> EspSession {
        EspSession {
            outbound_spi: 0x1020_3040,
            inbound_spi: 0x1020_3040,
            outbound_auth_key: vec![0x11; 16],
            outbound_crypt_key: vec![0x22; 16],
            inbound_auth_key: vec![0x11; 16],
            inbound_crypt_key: vec![0x22; 16],
            iv: vec![0x33; AES_BLOCK_SIZE],
            sequence: 1,
        }
    }
}
