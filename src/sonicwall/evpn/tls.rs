use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use native_tls::{Protocol, TlsConnector, TlsStream};

use super::inject_evpn_z;

const TLS_RECORD_HEADER_LEN: usize = 5;
const MAX_PROXY_HEADER_LEN: usize = 64 * 1024;

#[derive(Clone, Debug)]
pub(crate) struct EvpnTlsConnectOptions<'a> {
    pub(crate) server: &'a str,
    pub(crate) port: u16,
    pub(crate) http_connect_proxy: Option<&'a str>,
    pub(crate) timeout: Duration,
    pub(crate) verify_server_cert: bool,
}

pub(crate) type EvpnTlsStream = TlsStream<ClientHelloPatchStream<TcpStream>>;

/// Establishes SonicWall's EVPN TLS underlay without invoking an installed client.
///
/// SMA 1000 routes a normal HTTPS ClientHello to the web portal. The EVPN listener is selected
/// by advertising the private compression method 0xec. TLS itself remains standards-compliant;
/// only the first plaintext ClientHello record is changed before the TLS provider sees a reply.
pub(crate) fn connect_evpn_tls(options: &EvpnTlsConnectOptions<'_>) -> Result<EvpnTlsStream> {
    if options.server.trim().is_empty() {
        bail!("SonicWall EVPN server cannot be empty");
    }
    let mut tcp = connect_tcp(options)?;
    tcp.set_read_timeout(Some(options.timeout))
        .context("failed to set SonicWall EVPN read timeout")?;
    tcp.set_write_timeout(Some(options.timeout))
        .context("failed to set SonicWall EVPN write timeout")?;
    if let Some(proxy) = options.http_connect_proxy {
        establish_http_connect(&mut tcp, proxy, options.server, options.port)?;
    }

    let connector = TlsConnector::builder()
        .min_protocol_version(Some(Protocol::Tlsv12))
        .danger_accept_invalid_certs(!options.verify_server_cert)
        .danger_accept_invalid_hostnames(!options.verify_server_cert)
        .build()
        .context("failed to build SonicWall EVPN TLS connector")?;
    connector
        .connect(options.server, ClientHelloPatchStream::new(tcp))
        .map_err(|error| {
            anyhow::anyhow!("failed to complete SonicWall EVPN TLS handshake: {error}")
        })
}

fn connect_tcp(options: &EvpnTlsConnectOptions<'_>) -> Result<TcpStream> {
    let destination = options.http_connect_proxy.unwrap_or(options.server);
    let (host, port) = if options.http_connect_proxy.is_some() {
        parse_host_port(destination)?
    } else {
        (destination.to_string(), options.port)
    };
    let addresses = (host.as_str(), port)
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve SonicWall EVPN underlay {host}:{port}"))?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        bail!("SonicWall EVPN underlay {host}:{port} resolved to no addresses");
    }
    let mut last_error = None;
    for address in addresses {
        match TcpStream::connect_timeout(&address, options.timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error
        .map(anyhow::Error::from)
        .unwrap_or_else(|| anyhow::anyhow!("failed to connect"))
        .context(format!(
            "failed to connect to SonicWall EVPN underlay {host}:{port}"
        )))
}

fn parse_host_port(value: &str) -> Result<(String, u16)> {
    let value = value.trim();
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .context("bracketed HTTP CONNECT proxy must include a port")?;
        (host, port)
    } else {
        value
            .rsplit_once(':')
            .context("HTTP CONNECT proxy must be host:port")?
    };
    if host.trim().is_empty() {
        bail!("HTTP CONNECT proxy host cannot be empty");
    }
    let port = port
        .parse::<u16>()
        .context("HTTP CONNECT proxy port is invalid")?;
    if port == 0 {
        bail!("HTTP CONNECT proxy port cannot be zero");
    }
    Ok((host.to_string(), port))
}

fn establish_http_connect(
    stream: &mut TcpStream,
    proxy: &str,
    server: &str,
    port: u16,
) -> Result<()> {
    let request = format!(
        "CONNECT {server}:{port} HTTP/1.1\r\nHost: {server}:{port}\r\nProxy-Connection: keep-alive\r\n\r\n"
    );
    stream
        .write_all(request.as_bytes())
        .with_context(|| format!("failed to write HTTP CONNECT request to {proxy}"))?;
    stream
        .flush()
        .context("failed to flush HTTP CONNECT request")?;

    let mut response = Vec::new();
    while !response.ends_with(b"\r\n\r\n") {
        let mut byte = [0_u8; 1];
        stream
            .read_exact(&mut byte)
            .with_context(|| format!("HTTP CONNECT proxy {proxy} closed before its response"))?;
        response.push(byte[0]);
        if response.len() > MAX_PROXY_HEADER_LEN {
            bail!("HTTP CONNECT proxy response exceeds safety limit");
        }
    }
    let status_line = response
        .split(|byte| *byte == b'\n')
        .next()
        .context("HTTP CONNECT proxy returned an empty response")?;
    let status_line = String::from_utf8_lossy(status_line);
    let status = status_line
        .split_ascii_whitespace()
        .nth(1)
        .context("HTTP CONNECT proxy returned a malformed status line")?;
    if status != "200" {
        bail!("HTTP CONNECT proxy rejected SonicWall EVPN tunnel with status {status}");
    }
    Ok(())
}

#[derive(Debug)]
pub(crate) struct ClientHelloPatchStream<S> {
    inner: S,
    pending: Vec<u8>,
    patched: bool,
}

impl<S> ClientHelloPatchStream<S> {
    fn new(inner: S) -> Self {
        Self {
            inner,
            pending: Vec::new(),
            patched: false,
        }
    }

    #[cfg(test)]
    fn into_inner(self) -> S {
        self.inner
    }
}

impl ClientHelloPatchStream<TcpStream> {
    pub(crate) fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.inner.set_read_timeout(timeout)
    }
}

impl<S: Write> ClientHelloPatchStream<S> {
    fn forward_client_hello_if_complete(&mut self) -> io::Result<bool> {
        if self.patched {
            return Ok(true);
        }
        if self.pending.len() < TLS_RECORD_HEADER_LEN {
            return Ok(false);
        }
        let record_len = u16::from_be_bytes([self.pending[3], self.pending[4]]) as usize;
        let complete_len = TLS_RECORD_HEADER_LEN + record_len;
        if self.pending.len() < complete_len {
            return Ok(false);
        }

        let trailing = self.pending.split_off(complete_len);
        inject_evpn_z(&mut self.pending)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        self.inner.write_all(&self.pending)?;
        self.inner.write_all(&trailing)?;
        self.pending.clear();
        self.patched = true;
        Ok(true)
    }

    fn require_forwarded_client_hello(&mut self) -> io::Result<()> {
        if self.forward_client_hello_if_complete()? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "TLS provider attempted I/O before completing its ClientHello record",
            ))
        }
    }
}

impl<S: Read + Write> Read for ClientHelloPatchStream<S> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.require_forwarded_client_hello()?;
        self.inner.read(buffer)
    }
}

impl<S: Write> Write for ClientHelloPatchStream<S> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if self.patched {
            return self.inner.write(buffer);
        }
        self.pending.extend_from_slice(buffer);
        self.forward_client_hello_if_complete()?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.require_forwarded_client_hello()?;
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{ClientHelloPatchStream, EvpnTlsConnectOptions, connect_evpn_tls, parse_host_port};
    use std::io::{Read, Write};
    use std::time::Duration;

    fn client_hello() -> Vec<u8> {
        let session_id = [0xaa, 0xbb];
        let ciphers = [0x13, 0x01];
        let compression = [0x00];
        let handshake_len = 2 + 32 + 1 + session_id.len() + 2 + ciphers.len() + 1 + 1;
        let mut record = vec![0x16, 0x03, 0x01, 0, (handshake_len + 4) as u8, 0x01];
        record.extend_from_slice(&[
            ((handshake_len >> 16) & 0xff) as u8,
            ((handshake_len >> 8) & 0xff) as u8,
            (handshake_len & 0xff) as u8,
        ]);
        record.extend_from_slice(&[0x03, 0x03]);
        record.extend_from_slice(&[0x11; 32]);
        record.push(session_id.len() as u8);
        record.extend_from_slice(&session_id);
        record.extend_from_slice(&(ciphers.len() as u16).to_be_bytes());
        record.extend_from_slice(&ciphers);
        record.push(compression.len() as u8);
        record.extend_from_slice(&compression);
        record
    }

    #[test]
    fn patches_a_split_client_hello_before_forwarding() {
        let hello = client_hello();
        let mut stream = ClientHelloPatchStream::new(Vec::<u8>::new());
        stream.write_all(&hello[..20]).unwrap();
        assert!(stream.inner.is_empty());
        stream.write_all(&hello[20..]).unwrap();
        let output = stream.into_inner();
        assert_eq!(output.len(), hello.len() + 1);
        assert_eq!(output[3..5], [0, (hello.len() - 5 + 1) as u8]);
        assert!(output.windows(2).any(|window| window == [2, 0xec]));
    }

    #[test]
    fn preserves_bytes_after_the_first_tls_record() {
        let mut input = client_hello();
        input.extend_from_slice(&[0x14, 0x03, 0x03, 0, 1, 1]);
        let mut stream = ClientHelloPatchStream::new(Vec::<u8>::new());
        stream.write_all(&input).unwrap();
        let output = stream.into_inner();
        assert_eq!(&output[output.len() - 6..], &[0x14, 0x03, 0x03, 0, 1, 1]);
    }

    #[test]
    fn parses_proxy_host_and_port() {
        assert_eq!(
            parse_host_port("127.0.0.1:6780").unwrap(),
            ("127.0.0.1".into(), 6780)
        );
        assert_eq!(parse_host_port("[::1]:8080").unwrap(), ("::1".into(), 8080));
        assert!(parse_host_port("missing-port").is_err());
    }

    #[test]
    #[ignore = "requires explicit live network access"]
    fn live_gateway_accepts_native_tls_client_hello() {
        let server = std::env::var("SONICWALL_EVPN_SERVER")
            .unwrap_or_else(|_| "sslvpn.hundsun.com".to_string());
        let proxy = std::env::var("SONICWALL_EVPN_PROXY").ok();
        let mut stream = connect_evpn_tls(&EvpnTlsConnectOptions {
            server: &server,
            port: 443,
            http_connect_proxy: proxy.as_deref(),
            timeout: Duration::from_secs(20),
            verify_server_cert: true,
        })
        .unwrap();
        assert!(stream.peer_certificate().unwrap().is_some());
        stream
            .get_ref()
            .inner
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut first = [0_u8; 4096];
        match stream.read(&mut first) {
            Ok(length) => eprintln!("first EVPN bytes: {}", hex(&first[..length])),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                eprintln!("gateway sent no EVPN bytes before the client spoke")
            }
            Err(error) => panic!("failed reading first EVPN bytes: {error}"),
        }
        let version = super::super::encode_version(1, 2, 0, b"sing-box-tui").unwrap();
        stream.write_all(&version).unwrap();
        let length = stream.read(&mut first).unwrap();
        eprintln!("EVPN reply to VERSION: {}", hex(&first[..length]));
        assert!(length >= 4);
        if std::env::var_os("SONICWALL_EVPN_TEST_INVALID_TEAM").is_some() {
            let variant = std::env::var("SONICWALL_EVPN_TEAM_VARIANT")
                .unwrap_or_else(|_| "current".to_string());
            let team = invalid_team_probe(&variant);
            eprintln!("EVPN invalid TEAM variant: {variant}");
            stream.write_all(&team).unwrap();
            let length = stream.read(&mut first).unwrap();
            eprintln!("EVPN reply to invalid TEAM: {}", hex(&first[..length]));
        }
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn invalid_team_probe(variant: &str) -> Vec<u8> {
        use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

        let token = [0_u8; 16];
        match variant {
            "raw_only" => {
                super::super::encode_frame(super::super::MessageType::TEAM, 0, &token).unwrap()
            }
            "b64_only" => super::super::encode_frame(
                super::super::MessageType::TEAM,
                0,
                BASE64.encode(token).as_bytes(),
            )
            .unwrap(),
            "b64_tlv_only" => {
                let value = BASE64.encode(token);
                string_record_team(value.as_bytes(), &[])
            }
            "b64_tlv_group" => {
                let value = BASE64.encode(token);
                string_record_team(value.as_bytes(), b"Hundsun")
            }
            "raw_tlv_only" => string_record_team(&token, &[]),
            "current" => super::super::encode_team(&token, b"Hundsun").unwrap(),
            other => panic!("unknown SONICWALL_EVPN_TEAM_VARIANT {other}"),
        }
    }

    fn string_record_team(token: &[u8], team_name: &[u8]) -> Vec<u8> {
        let token_padded = (token.len() + 3) & !3;
        let name_padded = if team_name.is_empty() {
            0
        } else {
            (team_name.len() + 3) & !3
        };
        let payload_len = if team_name.is_empty() {
            4 + token_padded
        } else {
            14 + token_padded + name_padded
        };
        let mut payload = vec![0_u8; payload_len];
        payload[0..2].copy_from_slice(&(token.len() as u16).to_be_bytes());
        payload[2..4].copy_from_slice(&(token_padded as u16).to_be_bytes());
        payload[4..4 + token.len()].copy_from_slice(token);
        if !team_name.is_empty() {
            let cursor = 4 + token_padded;
            payload[cursor..cursor + 4].copy_from_slice(&[0, 1, 0, 1]);
            payload[cursor + 6..cursor + 8]
                .copy_from_slice(&(team_name.len() as u16).to_be_bytes());
            payload[cursor + 8..cursor + 10].copy_from_slice(&(name_padded as u16).to_be_bytes());
            payload[cursor + 10..cursor + 10 + team_name.len()].copy_from_slice(team_name);
        }
        super::super::encode_frame(super::super::MessageType::TEAM, 0, &payload).unwrap()
    }
}
