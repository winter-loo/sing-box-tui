use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::Url;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use urlencoding::encode;

use crate::import::build_full_config_from_singbox_subscription;

pub(crate) fn run_provider_sync(
    provider: String,
    account_file: &Path,
    config_path: &Path,
    output: Option<&PathBuf>,
    subscription_output: Option<&PathBuf>,
    replace_nodes: bool,
    write: bool,
) -> Result<()> {
    let credentials = ProviderCredentials::from_file(account_file)?;
    let provider_url = normalize_provider_url(&provider)?;
    let runtime = TokioRuntimeBuilder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build Tokio runtime for provider sync")?;
    let runtime_provider_url = provider_url.clone();

    let result = runtime.block_on(async move {
        let provider_kind = ProviderKind::detect(&runtime_provider_url)?;
        let client = Client::builder()
            .build()
            .context("failed to build provider HTTP client")?;

        let cookies = provider_kind
            .login(&client, &runtime_provider_url, &credentials)
            .await?;
        let subscription_url = provider_kind
            .fetch_singbox_subscription_url(&client, &runtime_provider_url, &cookies)
            .await?;
        let subscription_json = client
            .get(subscription_url.clone())
            .header("Cookie", &cookies)
            .send()
            .await
            .context("failed to fetch sing-box subscription JSON")?
            .error_for_status()
            .context("provider rejected sing-box subscription request")?
            .text()
            .await
            .context("failed to read sing-box subscription response")?;
        Ok::<_, anyhow::Error>((subscription_url, subscription_json))
    })?;

    let (subscription_url, subscription_json) = result;
    if let Some(path) = subscription_output {
        fs::write(path, format!("{subscription_json}\n"))
            .with_context(|| format!("failed to write {}", path.display()))?;
    }

    let config_path_buf = config_path.to_path_buf();
    let (config, imported_nodes) = build_full_config_from_singbox_subscription(
        &config_path_buf,
        &subscription_json,
        replace_nodes,
    )?;

    let merged_path = if let Some(path) = output.cloned() {
        path
    } else if write {
        config_path_buf
    } else {
        bail!("sync requires either --output <FILE> or --write");
    };
    fs::write(
        &merged_path,
        format!(
            "{}\n",
            serde_json::to_string_pretty(&config)
                .context("failed to serialize merged provider config")?
        ),
    )
    .with_context(|| format!("failed to write {}", merged_path.display()))?;

    println!(
        "{}",
        serde_json::to_string_pretty(&json!(ProviderSyncOutput {
            provider: provider_url.to_string(),
            singbox_subscription_url: subscription_url.to_string(),
            imported_nodes,
            merged_config_path: merged_path.display().to_string(),
            subscription_output_path: subscription_output
                .map(|path| path.display().to_string()),
        }))?
    );
    Ok(())
}

#[derive(Serialize)]
struct ProviderSyncOutput {
    provider: String,
    singbox_subscription_url: String,
    imported_nodes: usize,
    merged_config_path: String,
    subscription_output_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProviderCredentials {
    username: String,
    password: String,
}

impl ProviderCredentials {
    fn from_file(path: &Path) -> Result<Self> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("failed to read account file {}", path.display()))?;
        Self::parse(&text)
    }

    fn parse(text: &str) -> Result<Self> {
        let lines = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            bail!("account file is empty");
        }

        if lines.iter().all(|line| line.contains('=')) {
            let mut username = None;
            let mut password = None;
            for line in lines {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key.trim() {
                    "email" | "username" | "user" | "account" => {
                        username = Some(value.trim().to_string())
                    }
                    "password" | "passwd" | "pass" => {
                        password = Some(value.trim().to_string())
                    }
                    _ => {}
                }
            }

            return Ok(Self {
                username: username.context("account file is missing email/username")?,
                password: password.context("account file is missing password")?,
            });
        }

        if lines.len() < 2 {
            bail!("account file must contain username/email on the first line and password on the second line");
        }

        Ok(Self {
            username: lines[0].to_string(),
            password: lines[1].to_string(),
        })
    }
}

enum ProviderKind {
    AirTcp,
}

impl ProviderKind {
    fn detect(provider_url: &Url) -> Result<Self> {
        let host = provider_url
            .host_str()
            .context("provider URL is missing a host")?;
        if host.contains("airtcp.") {
            Ok(Self::AirTcp)
        } else {
            bail!("unsupported provider host: {host}")
        }
    }

    async fn login(
        &self,
        client: &Client,
        provider_url: &Url,
        credentials: &ProviderCredentials,
    ) -> Result<String> {
        match self {
            Self::AirTcp => login_airtcp(client, provider_url, credentials).await,
        }
    }

    async fn fetch_singbox_subscription_url(
        &self,
        client: &Client,
        provider_url: &Url,
        cookies: &str,
    ) -> Result<Url> {
        match self {
            Self::AirTcp => {
                fetch_airtcp_singbox_subscription_url(client, provider_url, cookies).await
            }
        }
    }
}

#[derive(Deserialize)]
struct AirTcpLoginResponse {
    ret: i32,
    msg: String,
}

async fn login_airtcp(
    client: &Client,
    provider_url: &Url,
    credentials: &ProviderCredentials,
) -> Result<String> {
    let login_url = provider_url
        .join("/denglu")
        .context("failed to build AirTCP login URL")?;
    let body = format!(
        "email={}&passwd={}&code=&remember_me=on",
        encode(&credentials.username),
        encode(&credentials.password),
    );
    let response = client
        .post(login_url)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("failed to send provider login request")?;
    let cookies = capture_cookie_header(response.headers())?;
    let payload: AirTcpLoginResponse = response
        .error_for_status()
        .context("provider login request returned an error")?
        .json()
        .await
        .context("failed to decode provider login response")?;
    if payload.ret != 1 {
        bail!("provider login failed: {}", payload.msg);
    }
    Ok(cookies)
}

async fn fetch_airtcp_singbox_subscription_url(
    client: &Client,
    provider_url: &Url,
    cookies: &str,
) -> Result<Url> {
    let user_url = provider_url
        .join("/user")
        .context("failed to build provider user page URL")?;
    let user_html = client
        .get(user_url)
        .header("Cookie", cookies)
        .send()
        .await
        .context("failed to fetch provider user page")?
        .error_for_status()
        .context("provider user page returned an error")?
        .text()
        .await
        .context("failed to read provider user page")?;

    if let Some(url) = extract_subscription_url_from_text(provider_url, &user_html, "singbox=1")? {
        return Ok(url);
    }

    for asset_url in extract_script_asset_urls(provider_url, &user_html)? {
        let script = client
            .get(asset_url)
            .header("Cookie", cookies)
            .send()
            .await
            .context("failed to fetch provider user asset")?
            .error_for_status()
            .context("provider user asset returned an error")?
            .text()
            .await
            .context("failed to read provider user asset")?;
        if let Some(url) =
            extract_subscription_url_from_text(provider_url, &script, "singbox=1")?
        {
            return Ok(url);
        }
    }

    bail!("failed to find a sing-box subscription URL in the authenticated provider page")
}

fn normalize_provider_url(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).with_context(|| format!("invalid provider URL: {value}"))?;
    if url.path().is_empty() {
        url.set_path("/");
    }
    Ok(url)
}

fn capture_cookie_header(headers: &reqwest::header::HeaderMap) -> Result<String> {
    let cookies = headers
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .filter_map(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if cookies.is_empty() {
        bail!("provider login response did not include any cookies");
    }
    Ok(cookies.join("; "))
}

fn extract_script_asset_urls(base_url: &Url, html: &str) -> Result<Vec<Url>> {
    let mut urls = Vec::new();
    let mut remaining = html;
    while let Some(index) = remaining.find(r#"src=""#) {
        let after_src = &remaining[index + 5..];
        let Some(end) = after_src.find('"') else {
            break;
        };
        let candidate = &after_src[..end];
        if candidate.ends_with(".js") {
            urls.push(
                base_url
                    .join(candidate)
                    .with_context(|| format!("invalid asset URL in provider HTML: {candidate}"))?,
            );
        }
        remaining = &after_src[end + 1..];
    }
    Ok(urls)
}

fn extract_subscription_url_from_text(
    base_url: &Url,
    text: &str,
    marker: &str,
) -> Result<Option<Url>> {
    let Some(index) = text.find(marker) else {
        return Ok(None);
    };
    let bytes = text.as_bytes();
    let mut start = index;
    while start > 0 {
        let ch = bytes[start - 1] as char;
        if matches!(ch, '"' | '\'' | ' ' | '\n' | '\r' | '\t') {
            break;
        }
        start -= 1;
    }
    let mut end = index + marker.len();
    while end < bytes.len() {
        let ch = bytes[end] as char;
        if matches!(ch, '"' | '\'' | ' ' | '\n' | '\r' | '\t') {
            break;
        }
        end += 1;
    }
    let candidate = text[start..end]
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'))
        .replace("\\u002F", "/")
        .replace("\\/", "/");
    if candidate.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        base_url
            .join(&candidate)
            .or_else(|_| Url::parse(&candidate))
            .with_context(|| format!("invalid subscription URL candidate: {candidate}"))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        ProviderCredentials, extract_script_asset_urls, extract_subscription_url_from_text,
        normalize_provider_url,
    };

    #[test]
    fn parses_key_value_account_file() {
        let credentials = ProviderCredentials::parse(
            "email=your-email@example.com\npassword=your-password\n",
        )
        .expect("account file parses");

        assert_eq!(credentials.username, "your-email@example.com");
        assert_eq!(credentials.password, "your-password");
    }

    #[test]
    fn parses_two_line_account_file() {
        let credentials = ProviderCredentials::parse("your-email@example.com\nyour-password\n")
            .expect("account file parses");

        assert_eq!(credentials.username, "your-email@example.com");
        assert_eq!(credentials.password, "your-password");
    }

    #[test]
    fn extracts_script_assets_from_html() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let urls = extract_script_asset_urls(
            &base,
            r#"<script type="module" src="/assets/user-abc.js"></script><script src="/assets/index-def.js"></script>"#,
        )
        .expect("asset extraction succeeds");

        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].as_str(), "https://3.airtcp.me/assets/user-abc.js");
        assert_eq!(urls[1].as_str(), "https://3.airtcp.me/assets/index-def.js");
    }

    #[test]
    fn extracts_absolute_subscription_url_from_script() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let url = extract_subscription_url_from_text(
            &base,
            r#"const s="https://spring.mailrelay.us/link/abc?singbox=1";"#,
            "singbox=1",
        )
        .expect("subscription extraction succeeds")
        .expect("subscription URL exists");

        assert_eq!(url.as_str(), "https://spring.mailrelay.us/link/abc?singbox=1");
    }

    #[test]
    fn extracts_escaped_relative_subscription_url_from_script() {
        let base = normalize_provider_url("https://3.airtcp.me").expect("base URL");
        let url = extract_subscription_url_from_text(
            &base,
            r#"const s="\/api\/subscription?singbox=1";"#,
            "singbox=1",
        )
        .expect("subscription extraction succeeds")
        .expect("subscription URL exists");

        assert_eq!(url.as_str(), "https://3.airtcp.me/api/subscription?singbox=1");
    }
}
