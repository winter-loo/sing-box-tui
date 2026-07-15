use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

mod tls;

#[allow(unused_imports)]
pub(crate) use tls::{EvpnTlsConnectOptions, EvpnTlsStream, connect_evpn_tls};

const HEADER_LEN: usize = 4;
const EXTENDED_HEADER_LEN: usize = 12;
const VERSION_MASK: u8 = 0xf0;
const VERSION_1: u8 = 0x10;
const FLAG_EXTENDED: u8 = 0x01;
const FLAG_LZ4: u8 = 0x04;
const MAX_REASSEMBLY_LEN: usize = 4 * 1024 * 1024;
const EVPN_Z_METHOD: u8 = 0xec;
const MAX_TLS_CONNECT_ATTEMPTS: usize = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MessageType(u8);

impl MessageType {
    pub(crate) const DATA: Self = Self(1);
    pub(crate) const SHUTDOWN: Self = Self(2);
    pub(crate) const ALERT: Self = Self(4);
    pub(crate) const RESPONSE: Self = Self(7);
    pub(crate) const VERSION: Self = Self(10);
    pub(crate) const VERSION_ACK: Self = Self(11);
    pub(crate) const REALM_LIST_RESPONSE: Self = Self(13);
    pub(crate) const TEAM: Self = Self(16);
    pub(crate) const AUTH_REQ: Self = Self(17);
    pub(crate) const AUTH_RSP: Self = Self(18);
    pub(crate) const AUTH_ACK: Self = Self(19);
    pub(crate) const CAPEX: Self = Self(20);
    pub(crate) const CLIENT_VERSION: Self = Self(21);
    pub(crate) const CLIENT_VERSION_ACK: Self = Self(22);
    pub(crate) const CLIENT_ADDR_INFO: Self = Self(23);
    pub(crate) const CLIENT_CONFIG: Self = Self(24);
    pub(crate) const CLIENT_CONFIG_ACK: Self = Self(25);
    pub(crate) const ECHO_REQ: Self = Self(26);
    pub(crate) const ECHO_RSP: Self = Self(27);

    pub(crate) fn new(value: u8) -> Result<Self> {
        if !(1..=48).contains(&value) {
            bail!("invalid SonicWall EVPN message type {value}");
        }
        Ok(Self(value))
    }

    pub(crate) fn value(self) -> u8 {
        self.0
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::DATA => "DATA",
            Self::SHUTDOWN => "SHUTDOWN",
            Self::ALERT => "ALERT",
            Self::RESPONSE => "RESPONSE",
            Self::VERSION => "VERSION",
            Self::VERSION_ACK => "VERSION_ACK",
            Self::REALM_LIST_RESPONSE => "REALM_LIST_RESPONSE",
            Self::TEAM => "TEAM",
            Self::AUTH_REQ => "AUTH_REQ",
            Self::AUTH_RSP => "AUTH_RSP",
            Self::AUTH_ACK => "AUTH_ACK",
            Self::CAPEX => "CAPEX",
            Self::CLIENT_VERSION => "CLIENT_VERSION",
            Self::CLIENT_VERSION_ACK => "CLIENT_VERSION_ACK",
            Self::CLIENT_ADDR_INFO => "CLIENT_ADDR_INFO",
            Self::CLIENT_CONFIG => "CLIENT_CONFIG",
            Self::CLIENT_CONFIG_ACK => "CLIENT_CONFIG_ACK",
            Self::ECHO_REQ => "ECHO_REQ",
            Self::ECHO_RSP => "ECHO_RSP",
            _ => "UNKNOWN",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Frame {
    pub(crate) flags: u8,
    pub(crate) message_type: MessageType,
    pub(crate) payload: Vec<u8>,
}

#[derive(Debug)]
struct FragmentAssembly {
    flags: u8,
    message_type: MessageType,
    expected_len: usize,
    payload: Vec<u8>,
}

#[derive(Default)]
pub(crate) struct Decoder {
    buffer: Vec<u8>,
    fragments: BTreeMap<u16, FragmentAssembly>,
}

impl Decoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub(crate) fn next_frame(&mut self) -> Result<Option<Frame>> {
        loop {
            if self.buffer.len() < HEADER_LEN {
                return Ok(None);
            }
            validate_header(&self.buffer[..HEADER_LEN])?;
            let total_len = u16::from_be_bytes([self.buffer[2], self.buffer[3]]) as usize;
            if self.buffer.len() < total_len {
                return Ok(None);
            }
            let raw = self.buffer.drain(..total_len).collect::<Vec<_>>();
            let flags = raw[0] & !VERSION_MASK;
            let message_type = MessageType::new(raw[1])?;
            if flags & FLAG_EXTENDED == 0 {
                return Ok(Some(Frame {
                    flags,
                    message_type,
                    payload: raw[HEADER_LEN..].to_vec(),
                }));
            }
            if let Some(frame) = self.consume_extended(flags, message_type, &raw)? {
                return Ok(Some(frame));
            }
        }
    }

    fn consume_extended(
        &mut self,
        flags: u8,
        message_type: MessageType,
        raw: &[u8],
    ) -> Result<Option<Frame>> {
        if raw.len() < EXTENDED_HEADER_LEN {
            bail!("SonicWall EVPN extended frame is shorter than 12 bytes");
        }
        let first = raw[5] == 1;
        let reassembly_id = u16::from_be_bytes([raw[6], raw[7]]);
        let expected_len = u32::from_be_bytes([raw[8], raw[9], raw[10], raw[11]]) as usize;
        if expected_len > MAX_REASSEMBLY_LEN {
            bail!("SonicWall EVPN fragmented payload exceeds safety limit");
        }
        let fragment = &raw[EXTENDED_HEADER_LEN..];
        if first {
            self.fragments.insert(
                reassembly_id,
                FragmentAssembly {
                    flags: flags & !FLAG_EXTENDED,
                    message_type,
                    expected_len,
                    payload: fragment.to_vec(),
                },
            );
        } else {
            let assembly = self
                .fragments
                .get_mut(&reassembly_id)
                .context("EVPN continuation fragment has no matching first fragment")?;
            if assembly.expected_len != expected_len || assembly.message_type != message_type {
                self.fragments.remove(&reassembly_id);
                bail!("SonicWall EVPN fragment metadata changed during reassembly");
            }
            assembly.payload.extend_from_slice(fragment);
        }

        let assembly = self
            .fragments
            .get(&reassembly_id)
            .context("SonicWall EVPN fragment assembly disappeared")?;
        if assembly.payload.len() > assembly.expected_len {
            self.fragments.remove(&reassembly_id);
            bail!("SonicWall EVPN fragmented payload exceeded declared length");
        }
        if assembly.payload.len() < assembly.expected_len {
            return Ok(None);
        }
        let assembly = self
            .fragments
            .remove(&reassembly_id)
            .context("SonicWall EVPN completed fragment assembly disappeared")?;
        Ok(Some(Frame {
            flags: assembly.flags,
            message_type: assembly.message_type,
            payload: assembly.payload,
        }))
    }
}

pub(crate) fn encode_frame(
    message_type: MessageType,
    flags: u8,
    payload: &[u8],
) -> Result<Vec<u8>> {
    if flags & VERSION_MASK != 0 {
        bail!("SonicWall EVPN flags cannot set version bits");
    }
    let total_len = HEADER_LEN
        .checked_add(payload.len())
        .context("SonicWall EVPN frame length overflow")?;
    let total_len = u16::try_from(total_len).context("SonicWall EVPN frame exceeds 65535 bytes")?;
    let mut output = Vec::with_capacity(total_len as usize);
    output.push(VERSION_1 | flags);
    output.push(message_type.value());
    output.extend_from_slice(&total_len.to_be_bytes());
    output.extend_from_slice(payload);
    Ok(output)
}

pub(crate) fn encode_data_packet(packet: &[u8]) -> Result<Vec<u8>> {
    validate_ip_packet(packet)?;
    encode_frame(MessageType::DATA, 0, packet)
}

pub(crate) fn decode_data_packet(frame: &Frame, max_packet_len: usize) -> Result<Vec<u8>> {
    if frame.message_type != MessageType::DATA {
        bail!("SonicWall EVPN frame is not a DATA message");
    }
    let packet = if frame.flags & FLAG_LZ4 != 0 {
        let mut output = vec![0_u8; max_packet_len];
        let length = lz4_flex::block::decompress_into(&frame.payload, &mut output)
            .context("failed to decompress SonicWall EVPN LZ4 block")?;
        output.truncate(length);
        output
    } else {
        if frame.payload.len() > max_packet_len {
            bail!("SonicWall EVPN DATA packet exceeds negotiated MTU");
        }
        frame.payload.clone()
    };
    validate_ip_packet(&packet)?;
    Ok(packet)
}

pub(crate) fn encode_version(
    major: u16,
    minor: u16,
    tunnel_id: u32,
    client_name: &[u8],
) -> Result<Vec<u8>> {
    let padded_len = padded_len(client_name.len())?;
    let total_len = 16_usize
        .checked_add(padded_len)
        .context("SonicWall EVPN VERSION length overflow")?;
    let total_len_u16 = u16::try_from(total_len).context("EVPN VERSION is too large")?;
    let name_len = u16::try_from(client_name.len()).context("EVPN client name is too large")?;
    let padded_len_u16 = u16::try_from(padded_len).context("EVPN client name is too large")?;
    let mut output = vec![0_u8; total_len];
    output[0] = VERSION_1;
    output[1] = MessageType::VERSION.value();
    output[2..4].copy_from_slice(&total_len_u16.to_be_bytes());
    output[4..6].copy_from_slice(&major.to_be_bytes());
    output[6..8].copy_from_slice(&minor.to_be_bytes());
    output[8..12].copy_from_slice(&tunnel_id.to_be_bytes());
    output[12..14].copy_from_slice(&name_len.to_be_bytes());
    output[14..16].copy_from_slice(&padded_len_u16.to_be_bytes());
    output[16..16 + client_name.len()].copy_from_slice(client_name);
    Ok(output)
}

pub(crate) fn encode_version_ack(tunnel_id: u32) -> Vec<u8> {
    let mut output = vec![VERSION_1, MessageType::VERSION_ACK.value(), 0, 8];
    output.extend_from_slice(&tunnel_id.to_be_bytes());
    output
}

pub(crate) fn parse_version_ack(frame: &Frame) -> Result<u32> {
    if frame.message_type != MessageType::VERSION_ACK || frame.payload.len() != 4 {
        bail!("SonicWall EVPN VERSION_ACK has an invalid type or length");
    }
    Ok(u32::from_be_bytes(
        frame.payload[..4]
            .try_into()
            .expect("VERSION_ACK payload length was checked"),
    ))
}

pub(crate) fn encode_team(team_token: &[u8; 16], client_id: &[u8]) -> Result<Vec<u8>> {
    let client_id_len = u16::try_from(client_id.len()).context("EVPN client id is too large")?;
    let client_id_padded_len = padded_len(client_id.len())?;
    let client_id_padded_len_u16 =
        u16::try_from(client_id_padded_len).context("EVPN padded client id is too large")?;
    let payload_len = 30_usize
        .checked_add(client_id_padded_len)
        .context("EVPN TEAM payload length overflow")?;
    let mut payload = vec![0_u8; payload_len];

    // SnwlEvpnProtocolMessages::CreateMTTeam sends the decoded logon id as a
    // fixed 16-byte record, followed by one client-id record. The managed
    // client passes IpcStartGuid as Base64 text; the native client preserves
    // that text in this record rather than placing the VPN realm here.
    payload[0..2].copy_from_slice(&16_u16.to_be_bytes());
    payload[2..4].copy_from_slice(&16_u16.to_be_bytes());
    payload[4..20].copy_from_slice(team_token);
    payload[20..22].copy_from_slice(&1_u16.to_be_bytes());
    payload[22..24].copy_from_slice(&1_u16.to_be_bytes());
    payload[24..26].copy_from_slice(&0_u16.to_be_bytes());
    payload[26..28].copy_from_slice(&client_id_len.to_be_bytes());
    payload[28..30].copy_from_slice(&client_id_padded_len_u16.to_be_bytes());
    payload[30..30 + client_id.len()].copy_from_slice(client_id);
    encode_frame(MessageType::TEAM, 0, &payload)
}

pub(crate) fn encode_client_version(major: u16, minor: u16, build: u32) -> Vec<u8> {
    let mut output = vec![VERSION_1, MessageType::CLIENT_VERSION.value(), 0, 12];
    output.extend_from_slice(&major.to_be_bytes());
    output.extend_from_slice(&minor.to_be_bytes());
    output.extend_from_slice(&build.to_be_bytes());
    output
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ClientAddressInterface {
    /// The seven address vectors used by the current (EVPN 1.2) address-info layout.
    pub(crate) address_sets: [Vec<IpAddr>; 7],
}

pub(crate) struct ClientAddressInfo<'a> {
    pub(crate) last_ipv4: Option<Ipv4Addr>,
    pub(crate) last_ipv6: Option<Ipv6Addr>,
    pub(crate) guid: [u8; 16],
    pub(crate) os_name: &'a [u8],
    pub(crate) interfaces: &'a [ClientAddressInterface],
    pub(crate) amid: &'a [u8],
}

/// Encodes the address-info layout selected by EVPN protocol version 1.2.
pub(crate) fn encode_client_address_info(info: &ClientAddressInfo<'_>) -> Result<Vec<u8>> {
    let interface_count = u8::try_from(info.interfaces.len())
        .context("SonicWall EVPN supports at most 255 local interfaces")?;
    let os_padded_len = padded_len(info.os_name.len())?;
    let amid_padded_len = if info.amid.is_empty() {
        0
    } else {
        padded_len(info.amid.len())?
    };
    let mut address_bytes = 0_usize;
    for interface in info.interfaces {
        address_bytes = address_bytes
            .checked_add(8)
            .context("SonicWall EVPN address-info length overflow")?;
        for addresses in &interface.address_sets {
            u8::try_from(addresses.len())
                .context("SonicWall EVPN address set exceeds 255 entries")?;
            address_bytes = address_bytes
                .checked_add(
                    addresses
                        .len()
                        .checked_mul(20)
                        .context("SonicWall EVPN address-info length overflow")?,
                )
                .context("SonicWall EVPN address-info length overflow")?;
        }
    }
    let amid_record_len = if info.amid.is_empty() {
        0
    } else {
        8_usize
            .checked_add(amid_padded_len)
            .context("SonicWall EVPN address-info length overflow")?
    };
    let total_len = 48_usize
        .checked_add(os_padded_len)
        .and_then(|length| length.checked_add(address_bytes))
        .and_then(|length| length.checked_add(amid_record_len))
        .context("SonicWall EVPN address-info length overflow")?;
    let total_len_u16 =
        u16::try_from(total_len).context("SonicWall EVPN CLIENT_ADDR_INFO is too large")?;
    let os_len = u16::try_from(info.os_name.len()).context("EVPN OS name is too large")?;
    let os_padded_len_u16 = u16::try_from(os_padded_len).context("EVPN OS name is too large")?;

    let mut output = vec![0_u8; total_len];
    output[0] = VERSION_1;
    output[1] = MessageType::CLIENT_ADDR_INFO.value();
    output[2..4].copy_from_slice(&total_len_u16.to_be_bytes());
    output[4] = interface_count;
    output[5] = match (info.last_ipv4.is_some(), info.last_ipv6.is_some()) {
        (true, false) => 2,
        (false, true) => 10,
        _ => 0,
    };
    if let Some(address) = info.last_ipv6 {
        output[8..24].copy_from_slice(&address.octets());
    }
    if let Some(address) = info.last_ipv4 {
        output[24..28].copy_from_slice(&address.octets());
    }
    output[28..44].copy_from_slice(&info.guid);
    output[44..46].copy_from_slice(&os_len.to_be_bytes());
    output[46..48].copy_from_slice(&os_padded_len_u16.to_be_bytes());
    output[48..48 + info.os_name.len()].copy_from_slice(info.os_name);

    let mut cursor = 48 + os_padded_len;
    for interface in info.interfaces {
        for (index, addresses) in interface.address_sets.iter().enumerate() {
            output[cursor + index] = addresses.len() as u8;
        }
        cursor += 8;
        for addresses in &interface.address_sets {
            for address in addresses {
                match address {
                    IpAddr::V4(address) => {
                        output[cursor] = 2;
                        output[cursor + 4..cursor + 8].copy_from_slice(&address.octets());
                    }
                    IpAddr::V6(address) => {
                        output[cursor] = 10;
                        output[cursor + 4..cursor + 20].copy_from_slice(&address.octets());
                    }
                }
                cursor += 20;
            }
        }
    }
    if !info.amid.is_empty() {
        let amid_len = u16::try_from(info.amid.len()).context("EVPN AMID is too large")?;
        let amid_padded_len_u16 =
            u16::try_from(amid_padded_len).context("EVPN AMID is too large")?;
        output[cursor..cursor + 2].copy_from_slice(&1_u16.to_be_bytes());
        output[cursor + 4..cursor + 6].copy_from_slice(&amid_len.to_be_bytes());
        output[cursor + 6..cursor + 8].copy_from_slice(&amid_padded_len_u16.to_be_bytes());
        output[cursor + 8..cursor + 8 + info.amid.len()].copy_from_slice(info.amid);
    }
    Ok(output)
}

pub(crate) fn read_frame<R: Read>(reader: &mut R, decoder: &mut Decoder) -> Result<Frame> {
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if let Some(frame) = decoder.next_frame()? {
            return Ok(frame);
        }
        let length = reader
            .read(&mut buffer)
            .context("failed to read SonicWall EVPN TLS stream")?;
        if length == 0 {
            bail!("SonicWall EVPN TLS stream closed while waiting for a frame");
        }
        decoder.push(&buffer[..length]);
    }
}

pub(crate) struct EvpnBootstrapOptions<'a> {
    pub(crate) server: &'a str,
    pub(crate) port: u16,
    pub(crate) http_connect_proxy: Option<&'a str>,
    pub(crate) timeout: Duration,
    pub(crate) verify_server_cert: bool,
    pub(crate) team_token: &'a [u8; 16],
    pub(crate) guid: [u8; 16],
    pub(crate) trace: Option<&'a dyn Fn(&str)>,
}

pub(crate) struct EstablishedEvpn {
    pub(crate) stream: EvpnTlsStream,
    pub(crate) decoder: Decoder,
    pub(crate) tunnel_id: u32,
    pub(crate) config: NetworkConfig,
}

pub(crate) fn connect_and_bootstrap(options: &EvpnBootstrapOptions<'_>) -> Result<EstablishedEvpn> {
    let tls_options = EvpnTlsConnectOptions {
        server: options.server,
        port: options.port,
        http_connect_proxy: options.http_connect_proxy,
        timeout: options.timeout,
        verify_server_cert: options.verify_server_cert,
    };
    let mut stream = connect_evpn_tls_with_retry(&tls_options, options.trace)?;
    trace_bootstrap(options.trace, "EVPN TLS stream established");
    let version = encode_version(1, 2, 0, b"sing-box-tui")?;
    stream
        .write_all(&version)
        .context("failed to send SonicWall EVPN VERSION")?;
    trace_bootstrap(options.trace, "sent VERSION 1.2");

    let mut decoder = Decoder::default();
    let version_ack = read_control_frame(&mut stream, &mut decoder)?;
    let tunnel_id = parse_version_ack(&version_ack)?;
    trace_bootstrap(
        options.trace,
        &format!("received VERSION_ACK tunnel_id=0x{tunnel_id:08x}"),
    );
    let client_id = BASE64.encode(options.guid);
    stream
        .write_all(&encode_team(options.team_token, client_id.as_bytes())?)
        .context("failed to send SonicWall EVPN TEAM")?;
    trace_bootstrap(options.trace, "sent TEAM layout=native-raw-token-client-id");

    let capabilities = modern_tls_capabilities(&options.guid);
    let capex = encode_capex(&capabilities)?;
    let address_info = encode_client_address_info(&ClientAddressInfo {
        last_ipv4: None,
        last_ipv6: None,
        guid: options.guid,
        os_name: b"Windows",
        interfaces: &[],
        amid: &[],
    })?;
    let mut sent_capex = false;
    let mut sent_address_info = false;
    let mut observed_types = Vec::new();

    for _ in 0..32 {
        let frame = read_control_frame(&mut stream, &mut decoder)?;
        observed_types.push(frame.message_type.value());
        trace_bootstrap(
            options.trace,
            &format!(
                "received {}({}) payload_len={}",
                frame.message_type.name(),
                frame.message_type.value(),
                frame.payload.len()
            ),
        );
        match frame.message_type {
            MessageType::SHUTDOWN | MessageType::ALERT => {
                let detail = decode_control_error(&frame.payload)
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default();
                let payload_hex = hex_preview(&frame.payload, 96);
                bail!(
                    "SonicWall EVPN gateway rejected tunnel bootstrap with message {}{} ({} payload bytes, payload_hex={}) after {:?}",
                    frame.message_type.value(),
                    detail,
                    frame.payload.len(),
                    payload_hex,
                    observed_types
                )
            }
            MessageType::AUTH_REQ
            | MessageType::AUTH_RSP
            | MessageType::AUTH_ACK
            | MessageType::RESPONSE
            | MessageType::REALM_LIST_RESPONSE => {
                if !sent_capex {
                    stream
                        .write_all(&capex)
                        .context("failed to send SonicWall EVPN CAPEX")?;
                    sent_capex = true;
                    trace_bootstrap(options.trace, "sent CAPEX");
                }
            }
            MessageType::CAPEX => {
                if !sent_capex {
                    stream
                        .write_all(&capex)
                        .context("failed to send SonicWall EVPN CAPEX")?;
                    sent_capex = true;
                    trace_bootstrap(options.trace, "sent CAPEX");
                }
                if !sent_address_info {
                    stream
                        .write_all(&address_info)
                        .context("failed to send SonicWall EVPN CLIENT_ADDR_INFO")?;
                    sent_address_info = true;
                    trace_bootstrap(options.trace, "sent CLIENT_ADDR_INFO");
                }
            }
            MessageType::CLIENT_VERSION_ACK => {
                if !sent_address_info {
                    stream
                        .write_all(&address_info)
                        .context("failed to send SonicWall EVPN CLIENT_ADDR_INFO")?;
                    sent_address_info = true;
                    trace_bootstrap(options.trace, "sent CLIENT_ADDR_INFO");
                }
            }
            MessageType::CLIENT_CONFIG => {
                let config = match parse_client_config(&frame, ClientConfigLayout::Current) {
                    Ok(config) => config,
                    Err(current_error) => {
                        trace_bootstrap(
                            options.trace,
                            &format!(
                                "current CLIENT_CONFIG layout rejected: {current_error:#}; trying v1 layout"
                            ),
                        );
                        match parse_client_config(&frame, ClientConfigLayout::V1) {
                            Ok(config) => {
                                trace_bootstrap(
                                    options.trace,
                                    "accepted CLIENT_CONFIG using v1 compatibility layout",
                                );
                                config
                            }
                            Err(v1_error) => {
                                trace_hex_chunks(
                                    options.trace,
                                    "CLIENT_CONFIG payload",
                                    &frame.payload,
                                    256,
                                );
                                bail!(
                                    "failed to parse SonicWall EVPN CLIENT_CONFIG with current layout ({current_error:#}) or v1 layout ({v1_error:#})"
                                )
                            }
                        }
                    }
                };
                stream
                    .write_all(&encode_frame(MessageType::CLIENT_CONFIG_ACK, 0, &[])?)
                    .context("failed to acknowledge SonicWall EVPN CLIENT_CONFIG")?;
                trace_bootstrap(
                    options.trace,
                    "received CLIENT_CONFIG and sent CLIENT_CONFIG_ACK",
                );
                return Ok(EstablishedEvpn {
                    stream,
                    decoder,
                    tunnel_id,
                    config,
                });
            }
            MessageType::ECHO_REQ => {
                stream
                    .write_all(&encode_frame(MessageType::ECHO_RSP, 0, &frame.payload)?)
                    .context("failed to answer SonicWall EVPN bootstrap keepalive")?;
                trace_bootstrap(options.trace, "answered ECHO_REQ");
            }
            _ => {}
        }
    }
    bail!(
        "SonicWall EVPN bootstrap exceeded the control-message safety limit; observed message types {:?}",
        observed_types
    )
}

fn connect_evpn_tls_with_retry(
    options: &EvpnTlsConnectOptions<'_>,
    trace: Option<&dyn Fn(&str)>,
) -> Result<EvpnTlsStream> {
    for attempt in 1..=MAX_TLS_CONNECT_ATTEMPTS {
        trace_bootstrap(
            trace,
            &format!("opening EVPN TLS stream attempt={attempt}/{MAX_TLS_CONNECT_ATTEMPTS}"),
        );
        match connect_evpn_tls(options) {
            Ok(stream) => return Ok(stream),
            Err(error)
                if attempt < MAX_TLS_CONNECT_ATTEMPTS && is_transient_tls_connect_error(&error) =>
            {
                trace_bootstrap(
                    trace,
                    &format!(
                        "transient EVPN TLS connection failure on attempt {attempt}; retrying"
                    ),
                );
                std::thread::sleep(Duration::from_millis(250 * attempt as u64));
            }
            Err(error) => return Err(error),
        }
    }
    unreachable!("EVPN TLS retry loop always returns on its final attempt")
}

fn is_transient_tls_connect_error(error: &anyhow::Error) -> bool {
    let message = format!("{error:#}").to_ascii_lowercase();
    [
        "os error 10053",
        "os error 10054",
        "os error 10060",
        "connection reset",
        "forcibly closed",
        "timed out",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

fn trace_bootstrap(trace: Option<&dyn Fn(&str)>, message: &str) {
    if let Some(trace) = trace {
        trace(message);
    }
}

fn trace_hex_chunks(trace: Option<&dyn Fn(&str)>, label: &str, bytes: &[u8], chunk_len: usize) {
    let chunk_len = chunk_len.max(1);
    for (index, chunk) in bytes.chunks(chunk_len).enumerate() {
        let start = index * chunk_len;
        let end = start + chunk.len();
        trace_bootstrap(
            trace,
            &format!(
                "{label}[{start:04x}..{end:04x}]={}",
                hex_preview(chunk, chunk.len())
            ),
        );
    }
}

fn decode_control_error(payload: &[u8]) -> Option<String> {
    if payload.len() < 8 {
        return None;
    }
    let message_len = u16::from_be_bytes([payload[4], payload[5]]) as usize;
    let padded_len = u16::from_be_bytes([payload[6], payload[7]]) as usize;
    if message_len == 0 || padded_len < message_len || payload.len() < 8 + padded_len {
        return None;
    }
    let message = &payload[8..8 + message_len];
    let message = message.strip_suffix(&[0]).unwrap_or(message);
    if message
        .iter()
        .all(|byte| byte.is_ascii_graphic() || *byte == b' ')
    {
        Some(String::from_utf8_lossy(message).into_owned())
    } else {
        None
    }
}

fn hex_preview(bytes: &[u8], max_len: usize) -> String {
    let shown = bytes.len().min(max_len);
    let mut output = String::with_capacity(shown.saturating_mul(2) + 3);
    for byte in &bytes[..shown] {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    if shown < bytes.len() {
        output.push_str("...");
    }
    output
}

fn read_control_frame(stream: &mut EvpnTlsStream, decoder: &mut Decoder) -> Result<Frame> {
    loop {
        let frame = read_frame(stream, decoder)?;
        if frame.message_type == MessageType::ECHO_REQ {
            stream
                .write_all(&encode_frame(MessageType::ECHO_RSP, 0, &frame.payload)?)
                .context("failed to answer SonicWall EVPN keepalive")?;
            continue;
        }
        return Ok(frame);
    }
}

fn modern_tls_capabilities(guid: &[u8; 16]) -> Vec<Capability> {
    let guid_text = BASE64.encode(guid);
    vec![
        Capability {
            id: 1,
            kind: 0,
            flags: 0,
            data: vec![1],
        },
        Capability {
            id: 7,
            kind: 0,
            flags: 0,
            data: vec![1],
        },
        Capability {
            id: 5,
            kind: 0,
            flags: 0,
            data: vec![0],
        },
        Capability {
            id: 8,
            kind: 0,
            flags: 0,
            data: vec![0],
        },
        Capability {
            id: 11,
            kind: 0,
            flags: 0,
            data: vec![0, 0],
        },
        Capability {
            id: 2,
            kind: 1,
            flags: 0,
            data: b"Windows".to_vec(),
        },
        Capability {
            id: 3,
            kind: 1,
            flags: 0,
            data: b"10.0.0".to_vec(),
        },
        Capability {
            id: 12,
            kind: 1,
            flags: 0,
            data: guid_text.into_bytes(),
        },
    ]
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Capability {
    pub(crate) id: u16,
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClientConfigLayout {
    V1,
    Current,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigAttribute {
    pub(crate) id: u16,
    pub(crate) kind: u8,
    pub(crate) flags: u8,
    pub(crate) data: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetworkConfig {
    pub(crate) flags: u16,
    pub(crate) address_mode: u8,
    pub(crate) assigned_ipv4: Ipv4Addr,
    pub(crate) ipv4_prefix_len: u8,
    pub(crate) assigned_ipv6: Option<Ipv6Addr>,
    pub(crate) interface_timeout: u32,
    pub(crate) dns: Vec<IpAddr>,
    pub(crate) wins: Vec<IpAddr>,
    pub(crate) exclusions: Vec<IpAddr>,
    pub(crate) suffixes: Vec<Vec<u8>>,
    pub(crate) resources: Vec<Vec<u8>>,
    pub(crate) attributes: Vec<ConfigAttribute>,
    pub(crate) ssl_mtu: Option<u16>,
}

pub(crate) fn parse_client_config(
    frame: &Frame,
    layout: ClientConfigLayout,
) -> Result<NetworkConfig> {
    if frame.message_type != MessageType::CLIENT_CONFIG {
        bail!("SonicWall EVPN frame is not CLIENT_CONFIG");
    }
    let bytes = frame.payload.as_slice();
    let fixed_len = match layout {
        ClientConfigLayout::V1 => 0x14,
        ClientConfigLayout::Current => 0x24,
    };
    if bytes.len() < fixed_len {
        bail!("SonicWall EVPN CLIENT_CONFIG fixed prefix is truncated");
    }
    let flags = read_u16(bytes, 0)?;
    let address_mode = bytes[2];
    let dns_count = bytes[3] as usize;
    let wins_count = bytes[4] as usize;
    let exclusion_count = bytes[5] as usize;
    let suffix_count = bytes[6] as usize;
    let prefix_from_wire = bytes[7];
    let resource_count = read_u16(bytes, 8)? as usize;
    let attribute_count = match layout {
        ClientConfigLayout::V1 => read_u16(bytes, 10)? as usize,
        ClientConfigLayout::Current => bytes[10] as usize,
    };
    let interface_timeout = read_u32(bytes, 12)?;
    let assigned_ipv4 = Ipv4Addr::new(bytes[16], bytes[17], bytes[18], bytes[19]);
    let (ipv4_prefix_len, assigned_ipv6) = match layout {
        ClientConfigLayout::V1 => (prefix_from_wire, None),
        ClientConfigLayout::Current => {
            let mut ipv6 = [0_u8; 16];
            ipv6.copy_from_slice(&bytes[20..36]);
            (32, (ipv6 != [0; 16]).then(|| Ipv6Addr::from(ipv6)))
        }
    };
    if ipv4_prefix_len > 32 {
        bail!("SonicWall EVPN CLIENT_CONFIG contains invalid IPv4 prefix length");
    }

    let mut cursor = fixed_len;
    let dns = parse_config_addresses(bytes, &mut cursor, dns_count, layout)?;
    let wins = parse_config_addresses(bytes, &mut cursor, wins_count, layout)?;
    let exclusions = parse_config_addresses(bytes, &mut cursor, exclusion_count, layout)?;
    let suffixes = parse_config_records(bytes, &mut cursor, suffix_count)?;
    let resources = parse_config_records(bytes, &mut cursor, resource_count)?;
    let attributes = parse_config_attributes(bytes, &mut cursor, attribute_count)?;
    if cursor != bytes.len() && bytes[cursor..].iter().any(|byte| *byte != 0) {
        bail!("SonicWall EVPN CLIENT_CONFIG has unparsed trailing data");
    }
    let ssl_mtu = attributes
        .iter()
        .find(|attribute| attribute.id == 11 && attribute.data.len() >= 2)
        .map(|attribute| u16::from_be_bytes([attribute.data[0], attribute.data[1]]))
        .filter(|mtu| (1200..=1500).contains(mtu));
    Ok(NetworkConfig {
        flags,
        address_mode,
        assigned_ipv4,
        ipv4_prefix_len,
        assigned_ipv6,
        interface_timeout,
        dns,
        wins,
        exclusions,
        suffixes,
        resources,
        attributes,
        ssl_mtu,
    })
}

fn parse_config_addresses(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
    layout: ClientConfigLayout,
) -> Result<Vec<IpAddr>> {
    let mut addresses = Vec::with_capacity(count);
    for _ in 0..count {
        match layout {
            ClientConfigLayout::V1 => {
                let end = cursor
                    .checked_add(4)
                    .context("CLIENT_CONFIG address length overflow")?;
                let value = bytes
                    .get(*cursor..end)
                    .context("SonicWall EVPN CLIENT_CONFIG IPv4 list is truncated")?;
                addresses.push(IpAddr::V4(Ipv4Addr::new(
                    value[0], value[1], value[2], value[3],
                )));
                *cursor = end;
            }
            ClientConfigLayout::Current => {
                let end = cursor
                    .checked_add(20)
                    .context("CLIENT_CONFIG address length overflow")?;
                let value = bytes
                    .get(*cursor..end)
                    .context("SonicWall EVPN CLIENT_CONFIG address list is truncated")?;
                let address = match value[0] {
                    2 => IpAddr::V4(Ipv4Addr::new(value[4], value[5], value[6], value[7])),
                    10 => {
                        let mut ipv6 = [0_u8; 16];
                        ipv6.copy_from_slice(&value[4..20]);
                        IpAddr::V6(Ipv6Addr::from(ipv6))
                    }
                    family => bail!("unsupported CLIENT_CONFIG address family {family}"),
                };
                addresses.push(address);
                *cursor = end;
            }
        }
    }
    Ok(addresses)
}

fn parse_config_records(bytes: &[u8], cursor: &mut usize, count: usize) -> Result<Vec<Vec<u8>>> {
    let mut records = Vec::with_capacity(count);
    for _ in 0..count {
        let header_end = cursor
            .checked_add(4)
            .context("CLIENT_CONFIG record length overflow")?;
        let header = bytes
            .get(*cursor..header_end)
            .context("SonicWall EVPN CLIENT_CONFIG record header is truncated")?;
        let length = u16::from_be_bytes([header[0], header[1]]) as usize;
        let padded = u16::from_be_bytes([header[2], header[3]]) as usize;
        if padded < length || padded > MAX_REASSEMBLY_LEN {
            bail!("invalid CLIENT_CONFIG record padded length");
        }
        let data_end = header_end
            .checked_add(length)
            .context("CLIENT_CONFIG record length overflow")?;
        let next = header_end
            .checked_add(padded)
            .context("CLIENT_CONFIG record length overflow")?;
        let data = bytes
            .get(header_end..data_end)
            .context("SonicWall EVPN CLIENT_CONFIG record data is truncated")?;
        bytes
            .get(data_end..next)
            .context("SonicWall EVPN CLIENT_CONFIG record padding is truncated")?;
        records.push(data.to_vec());
        *cursor = next;
    }
    Ok(records)
}

fn parse_config_attributes(
    bytes: &[u8],
    cursor: &mut usize,
    count: usize,
) -> Result<Vec<ConfigAttribute>> {
    let mut attributes = Vec::with_capacity(count);
    for _ in 0..count {
        let prefix = bytes
            .get(*cursor..)
            .filter(|value| value.len() >= 8)
            .context("SonicWall EVPN CLIENT_CONFIG attribute header is truncated")?;
        let id = u16::from_be_bytes([prefix[0], prefix[1]]);
        let kind = prefix[2];
        let flags = prefix[3];
        let (header_len, data_len, padded) = match kind {
            0 | 1 => (
                8,
                u16::from_be_bytes([prefix[4], prefix[5]]) as usize,
                u16::from_be_bytes([prefix[6], prefix[7]]) as usize,
            ),
            3 | 4 => {
                let prefix = bytes
                    .get(*cursor..)
                    .filter(|value| value.len() >= 12)
                    .context("CLIENT_CONFIG wide attribute header is truncated")?;
                (
                    12,
                    u32::from_be_bytes([prefix[4], prefix[5], prefix[6], prefix[7]]) as usize,
                    u32::from_be_bytes([prefix[8], prefix[9], prefix[10], prefix[11]]) as usize,
                )
            }
            _ => bail!("unsupported CLIENT_CONFIG attribute kind {kind}"),
        };
        if padded < data_len || padded > MAX_REASSEMBLY_LEN {
            bail!("invalid CLIENT_CONFIG attribute padded length");
        }
        let data_start = cursor
            .checked_add(header_len)
            .context("CLIENT_CONFIG attribute length overflow")?;
        let data_end = data_start
            .checked_add(data_len)
            .context("CLIENT_CONFIG attribute length overflow")?;
        let next = data_start
            .checked_add(padded)
            .context("CLIENT_CONFIG attribute length overflow")?;
        let data = bytes
            .get(data_start..data_end)
            .context("SonicWall EVPN CLIENT_CONFIG attribute data is truncated")?;
        bytes
            .get(data_end..next)
            .context("SonicWall EVPN CLIENT_CONFIG attribute padding is truncated")?;
        attributes.push(ConfigAttribute {
            id,
            kind,
            flags,
            data: data.to_vec(),
        });
        *cursor = next;
    }
    Ok(attributes)
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("SonicWall EVPN field is truncated")?;
    Ok(u16::from_be_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("SonicWall EVPN field is truncated")?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

pub(crate) fn encode_capex(capabilities: &[Capability]) -> Result<Vec<u8>> {
    let count = u16::try_from(capabilities.len()).context("too many EVPN capabilities")?;
    let mut payload = Vec::new();
    payload.extend_from_slice(&count.to_be_bytes());
    payload.extend_from_slice(&[0, 0]);
    for capability in capabilities {
        if capability.kind > 1 {
            bail!(
                "EVPN capability kind {} is not implemented",
                capability.kind
            );
        }
        let padded = padded_len(capability.data.len())?;
        let data_len =
            u16::try_from(capability.data.len()).context("EVPN capability data is too large")?;
        let padded_u16 = u16::try_from(padded).context("EVPN capability data is too large")?;
        payload.extend_from_slice(&capability.id.to_be_bytes());
        payload.push(capability.kind);
        payload.push(capability.flags);
        payload.extend_from_slice(&data_len.to_be_bytes());
        payload.extend_from_slice(&padded_u16.to_be_bytes());
        payload.extend_from_slice(&capability.data);
        payload.resize(payload.len() + padded - capability.data.len(), 0);
    }
    encode_frame(MessageType::CAPEX, 0, &payload)
}

pub(crate) fn inject_evpn_z(client_hello_record: &mut Vec<u8>) -> Result<bool> {
    if client_hello_record.len() < 44
        || client_hello_record[0] != 0x16
        || client_hello_record[5] != 0x01
    {
        bail!("buffer is not a complete TLS ClientHello record");
    }
    let record_len = u16::from_be_bytes([client_hello_record[3], client_hello_record[4]]) as usize;
    if record_len + 5 != client_hello_record.len() {
        bail!("TLS ClientHello record length does not match buffer");
    }
    let handshake_len = ((client_hello_record[6] as usize) << 16)
        | ((client_hello_record[7] as usize) << 8)
        | client_hello_record[8] as usize;
    if handshake_len + 4 != record_len {
        bail!("TLS ClientHello handshake length does not match record");
    }
    let session_len_offset = 9 + 2 + 32;
    let session_len = client_hello_record[session_len_offset] as usize;
    let cipher_len_offset = session_len_offset + 1 + session_len;
    if cipher_len_offset + 2 > client_hello_record.len() {
        bail!("TLS ClientHello session id exceeds record");
    }
    let cipher_len = u16::from_be_bytes([
        client_hello_record[cipher_len_offset],
        client_hello_record[cipher_len_offset + 1],
    ]) as usize;
    let compression_len_offset = cipher_len_offset + 2 + cipher_len;
    if compression_len_offset >= client_hello_record.len() {
        bail!("TLS ClientHello cipher list exceeds record");
    }
    let compression_len = client_hello_record[compression_len_offset] as usize;
    let compression_start = compression_len_offset + 1;
    let compression_end = compression_start + compression_len;
    if compression_end > client_hello_record.len() {
        bail!("TLS ClientHello compression methods exceed record");
    }
    if client_hello_record[compression_start..compression_end].contains(&EVPN_Z_METHOD) {
        return Ok(false);
    }
    if compression_len == u8::MAX as usize || record_len == u16::MAX as usize {
        bail!("TLS ClientHello cannot grow to include EVPN-Z marker");
    }
    let new_handshake_len = handshake_len
        .checked_add(1)
        .filter(|value| *value <= 0x00ff_ffff)
        .context("TLS ClientHello handshake is too large")?;
    client_hello_record.insert(compression_start, EVPN_Z_METHOD);
    client_hello_record[compression_len_offset] = (compression_len + 1) as u8;
    client_hello_record[3..5].copy_from_slice(&((record_len + 1) as u16).to_be_bytes());
    client_hello_record[6] = ((new_handshake_len >> 16) & 0xff) as u8;
    client_hello_record[7] = ((new_handshake_len >> 8) & 0xff) as u8;
    client_hello_record[8] = (new_handshake_len & 0xff) as u8;
    Ok(true)
}

fn validate_header(header: &[u8]) -> Result<()> {
    if header.len() < HEADER_LEN {
        bail!("SonicWall EVPN header is incomplete");
    }
    if header[0] & VERSION_MASK != VERSION_1 {
        bail!("unsupported SonicWall EVPN frame version");
    }
    MessageType::new(header[1])?;
    if u16::from_be_bytes([header[2], header[3]]) < HEADER_LEN as u16 {
        bail!("SonicWall EVPN frame length is smaller than its header");
    }
    Ok(())
}

fn validate_ip_packet(packet: &[u8]) -> Result<()> {
    let Some(first) = packet.first() else {
        bail!("SonicWall EVPN DATA packet is empty");
    };
    match first >> 4 {
        4 | 6 => Ok(()),
        version => bail!("SonicWall EVPN DATA payload has invalid IP version {version}"),
    }
}

fn padded_len(length: usize) -> Result<usize> {
    if length == 0 {
        return Ok(4);
    }
    length
        .checked_add(3)
        .map(|length| length & !3)
        .context("SonicWall EVPN padded length overflow")
}

#[cfg(test)]
mod tests {
    use super::{
        Capability, ClientAddressInfo, ClientAddressInterface, ClientConfigLayout, Decoder,
        FLAG_LZ4, Frame, MessageType, decode_control_error, decode_data_packet, encode_capex,
        encode_client_address_info, encode_client_version, encode_data_packet, encode_frame,
        encode_team, encode_version, encode_version_ack, inject_evpn_z,
        is_transient_tls_connect_error, modern_tls_capabilities, parse_client_config,
        parse_version_ack,
    };
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn ipv4_packet() -> Vec<u8> {
        vec![
            0x45, 0, 0, 20, 0, 0, 0, 0, 64, 1, 0, 0, 10, 0, 0, 1, 10, 0, 0, 2,
        ]
    }

    #[test]
    fn stream_decoder_handles_partial_and_concatenated_frames() {
        let first = encode_frame(MessageType::ECHO_REQ, 0, &[1, 2]).unwrap();
        let second = encode_frame(MessageType::ECHO_RSP, 0, &[3]).unwrap();
        let mut decoder = Decoder::default();
        decoder.push(&first[..3]);
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&first[3..]);
        decoder.push(&second);
        assert_eq!(decoder.next_frame().unwrap().unwrap().payload, [1, 2]);
        assert_eq!(
            decoder.next_frame().unwrap().unwrap().message_type,
            MessageType::ECHO_RSP
        );
    }

    #[test]
    fn data_frame_carries_raw_l3_packet_without_subheader() {
        let packet = ipv4_packet();
        let encoded = encode_data_packet(&packet).unwrap();
        assert_eq!(&encoded[..4], &[0x10, 1, 0, 24]);
        assert_eq!(&encoded[4..], packet);
    }

    #[test]
    fn raw_lz4_data_block_decodes_to_ip_packet() {
        let packet = ipv4_packet();
        let frame = Frame {
            flags: FLAG_LZ4,
            message_type: MessageType::DATA,
            payload: lz4_flex::block::compress(&packet),
        };
        assert_eq!(decode_data_packet(&frame, 1500).unwrap(), packet);
    }

    #[test]
    fn extended_fragments_are_reassembled_by_id() {
        fn fragment(first: bool, id: u16, total: u32, bytes: &[u8]) -> Vec<u8> {
            let mut payload = vec![0, u8::from(first)];
            payload.extend_from_slice(&id.to_be_bytes());
            payload.extend_from_slice(&total.to_be_bytes());
            payload.extend_from_slice(bytes);
            encode_frame(MessageType::CLIENT_CONFIG, 1, &payload).unwrap()
        }
        let mut decoder = Decoder::default();
        decoder.push(&fragment(true, 7, 5, &[1, 2]));
        assert!(decoder.next_frame().unwrap().is_none());
        decoder.push(&fragment(false, 7, 5, &[3, 4, 5]));
        let frame = decoder.next_frame().unwrap().unwrap();
        assert_eq!(frame.flags, 0);
        assert_eq!(frame.payload, [1, 2, 3, 4, 5]);
    }

    #[test]
    fn version_and_ack_match_known_layout() {
        let version = encode_version(1, 2, 0, b"ct").unwrap();
        assert_eq!(
            &version[..16],
            &[0x10, 10, 0, 20, 0, 1, 0, 2, 0, 0, 0, 0, 0, 2, 0, 4]
        );
        assert_eq!(&version[16..], &[b'c', b't', 0, 0]);
        assert_eq!(
            encode_version_ack(0x11223344),
            [0x10, 11, 0, 8, 0x11, 0x22, 0x33, 0x44]
        );
        let frame = Frame {
            flags: 0,
            message_type: MessageType::VERSION_ACK,
            payload: vec![0x11, 0x22, 0x33, 0x44],
        };
        assert_eq!(parse_version_ack(&frame).unwrap(), 0x11223344);
    }

    #[test]
    fn client_version_matches_legacy_v1_1_layout() {
        assert_eq!(
            encode_client_version(12, 5, 212),
            [0x10, 21, 0, 12, 0, 12, 0, 5, 0, 0, 0, 212]
        );
    }

    #[test]
    fn current_client_address_info_uses_twenty_byte_address_slots() {
        let interface = ClientAddressInterface {
            address_sets: [
                vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10))],
                vec![],
                vec![IpAddr::V6(Ipv6Addr::LOCALHOST)],
                vec![],
                vec![],
                vec![],
                vec![],
            ],
        };
        let encoded = encode_client_address_info(&ClientAddressInfo {
            last_ipv4: Some(Ipv4Addr::new(10, 22, 28, 59)),
            last_ipv6: None,
            guid: [0x5a; 16],
            os_name: b"Windows",
            interfaces: &[interface],
            amid: b"id",
        })
        .unwrap();
        assert_eq!(&encoded[..6], &[0x10, 23, 0, 116, 1, 2]);
        assert_eq!(&encoded[24..28], &[10, 22, 28, 59]);
        assert_eq!(&encoded[28..44], &[0x5a; 16]);
        assert_eq!(&encoded[44..48], &[0, 7, 0, 8]);
        assert_eq!(&encoded[56..64], &[1, 0, 1, 0, 0, 0, 0, 0]);
        assert_eq!(&encoded[64..72], &[2, 0, 0, 0, 192, 168, 1, 10]);
        assert_eq!(encoded[84], 10);
        assert_eq!(&encoded[88..104], &Ipv6Addr::LOCALHOST.octets());
        assert_eq!(&encoded[104..112], &[0, 1, 0, 0, 0, 2, 0, 4]);
    }

    #[test]
    fn team_matches_native_raw_token_and_base64_client_id_layout() {
        let token = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        let client_id = b"ABEiM0RVZneImaq7zN3u/w==";
        let team = encode_team(&token, client_id).unwrap();
        assert_eq!(&team[..8], &[0x10, 16, 0, 58, 0, 16, 0, 16]);
        assert_eq!(&team[8..24], &token);
        assert_eq!(&team[24..34], &[0, 1, 0, 1, 0, 0, 0, 24, 0, 24]);
        assert_eq!(&team[34..], client_id);
    }

    #[test]
    fn only_transient_tls_transport_failures_are_retried() {
        assert!(is_transient_tls_connect_error(&anyhow::anyhow!(
            "failed handshake: os error 10054"
        )));
        assert!(is_transient_tls_connect_error(&anyhow::anyhow!(
            "connection reset by peer"
        )));
        assert!(!is_transient_tls_connect_error(&anyhow::anyhow!(
            "certificate verify failed"
        )));
    }

    #[test]
    fn shutdown_payload_decodes_gateway_error_text() {
        let payload = b"\x00\x00\x00\x02\x00\x0f\x00\x10TEAM AUTH ERROR\x00";
        assert_eq!(
            decode_control_error(payload).as_deref(),
            Some("TEAM AUTH ERROR")
        );
        assert_eq!(decode_control_error(b"\x00\x00"), None);
    }

    #[test]
    fn capex_uses_count_and_padded_item16_records() {
        let frame = encode_capex(&[Capability {
            id: 8,
            kind: 0,
            flags: 0,
            data: vec![1],
        }])
        .unwrap();
        assert_eq!(&frame[..8], &[0x10, 20, 0, 20, 0, 1, 0, 0]);
        assert_eq!(&frame[8..16], &[0, 8, 0, 0, 0, 1, 0, 4]);
    }

    #[test]
    fn modern_capex_advertises_connect_tunnel_launch_mode() {
        let capabilities = modern_tls_capabilities(&[0x5a; 16]);
        let launch_mode = capabilities
            .iter()
            .find(|capability| capability.id == 7)
            .expect("CAPEX includes CAT_LAUNCH_MODE");
        assert_eq!(launch_mode.kind, 0);
        assert_eq!(launch_mode.flags, 0);
        assert_eq!(launch_mode.data, [1]);

        let lz4 = capabilities
            .iter()
            .find(|capability| capability.id == 8)
            .expect("CAPEX includes CAT_LZ4_COMPRESSION");
        assert_eq!(lz4.data, [0]);

        for id in [2, 3, 12] {
            let text = capabilities
                .iter()
                .find(|capability| capability.id == id)
                .expect("CAPEX includes the native text capability");
            assert_eq!(text.kind, 1, "capability {id} is encoded as a string");
            assert_eq!(text.flags, 0);
            assert!(!text.data.is_empty());
        }
    }

    #[test]
    fn client_hello_injects_private_compression_method_and_updates_lengths() {
        let mut body = vec![0x03, 0x03];
        body.extend_from_slice(&[0x11; 32]);
        body.push(0);
        body.extend_from_slice(&[0, 2, 0x13, 0x01]);
        body.extend_from_slice(&[1, 0]);
        let mut handshake = vec![1, 0, 0, body.len() as u8];
        handshake.extend_from_slice(&body);
        let mut record = vec![0x16, 0x03, 0x01, 0, handshake.len() as u8];
        record.extend_from_slice(&handshake);

        assert!(inject_evpn_z(&mut record).unwrap());
        assert_eq!(record[3..5], [0, 46]);
        assert_eq!(record[6..9], [0, 0, 42]);
        assert_eq!(&record[48..51], &[2, 0xec, 0]);
        assert!(!inject_evpn_z(&mut record).unwrap());
    }

    #[test]
    fn v1_client_config_decodes_ipv4_lists_records_and_ssl_mtu() {
        let mut payload = vec![
            0x12, 0x34, // flags
            1,    // address mode
            1,    // DNS count
            0,    // WINS count
            1,    // exclusion count
            1,    // suffix count
            24,   // prefix
            0, 1, // resource count
            0, 1, // attribute count
            0, 0, 0, 30, // interface timeout
            10, 22, 28, 59, // assigned IPv4
        ];
        payload.extend_from_slice(&[10, 22, 1, 6]);
        payload.extend_from_slice(&[192, 168, 60, 10]);
        payload.extend_from_slice(&[0, 3, 0, 4, b'l', b'a', b'b', 0]);
        payload.extend_from_slice(&[0, 2, 0, 4, 0xaa, 0xbb, 0, 0]);
        payload.extend_from_slice(&[0, 11, 0, 0, 0, 2, 0, 4, 0x05, 0xdc, 0, 0]);
        let frame = Frame {
            flags: 0,
            message_type: MessageType::CLIENT_CONFIG,
            payload,
        };

        let config = parse_client_config(&frame, ClientConfigLayout::V1).unwrap();
        assert_eq!(config.flags, 0x1234);
        assert_eq!(config.assigned_ipv4, Ipv4Addr::new(10, 22, 28, 59));
        assert_eq!(config.ipv4_prefix_len, 24);
        assert_eq!(config.dns, [IpAddr::V4(Ipv4Addr::new(10, 22, 1, 6))]);
        assert_eq!(
            config.exclusions,
            [IpAddr::V4(Ipv4Addr::new(192, 168, 60, 10))]
        );
        assert_eq!(config.suffixes, [b"lab".to_vec()]);
        assert_eq!(config.resources, [vec![0xaa, 0xbb]]);
        assert_eq!(config.ssl_mtu, Some(1500));
    }

    #[test]
    fn current_client_config_decodes_twenty_byte_ip_slots() {
        let assigned_ipv6 = Ipv6Addr::LOCALHOST.octets();
        let mut payload = vec![
            0, 0, // flags
            1, // address mode
            2, // DNS count
            0, // WINS count
            0, // exclusion count
            0, // suffix count
            0, // ignored prefix in current layout
            0, 0, // resources
            1, 0, // one-byte attribute count and reserved byte
            0, 0, 0, 10, // timeout
            10, 22, 28, 59, // IPv4
        ];
        payload.extend_from_slice(&assigned_ipv6);
        payload.extend_from_slice(&[2, 0, 0, 0, 10, 22, 1, 6]);
        payload.extend_from_slice(&[0; 12]);
        payload.extend_from_slice(&[10, 0, 0, 0]);
        payload.extend_from_slice(&Ipv6Addr::LOCALHOST.octets());
        payload.extend_from_slice(&[0, 11, 0, 0, 0, 2, 0, 4, 0x05, 0x94, 0, 0]);
        let frame = Frame {
            flags: 0,
            message_type: MessageType::CLIENT_CONFIG,
            payload,
        };

        let config = parse_client_config(&frame, ClientConfigLayout::Current).unwrap();
        assert_eq!(config.ipv4_prefix_len, 32);
        assert_eq!(config.assigned_ipv6, Some(Ipv6Addr::LOCALHOST));
        assert_eq!(
            config.dns,
            [
                IpAddr::V4(Ipv4Addr::new(10, 22, 1, 6)),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
            ]
        );
        assert_eq!(config.attributes.len(), 1);
        assert_eq!(config.ssl_mtu, Some(1428));
    }
}
