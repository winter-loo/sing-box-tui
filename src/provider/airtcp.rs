use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::Url;
use serde::Deserialize;
use urlencoding::encode;

use super::{
    BoxFuture, ProviderCredentials, ProviderPlugin, SubscriptionFormat, capture_cookie_header,
    extract_script_asset_urls, extract_subscription_url_from_text,
};

pub(super) static AIRTCP_PLUGIN: AirTcpPlugin = AirTcpPlugin;

pub(super) struct AirTcpPlugin;

impl ProviderPlugin for AirTcpPlugin {
    fn key(&self) -> &'static str {
        "airtcp"
    }

    fn matches(&self, provider_url: &Url) -> bool {
        provider_url
            .host_str()
            .is_some_and(|host| host.contains("airtcp."))
    }

    fn login<'a>(
        &'a self,
        client: &'a Client,
        provider_url: &'a Url,
        credentials: &'a ProviderCredentials,
    ) -> BoxFuture<'a, String> {
        Box::pin(async move { login_airtcp(client, provider_url, credentials).await })
    }

    fn fetch_subscription_url<'a>(
        &'a self,
        client: &'a Client,
        provider_url: &'a Url,
        cookies: &'a str,
        format: SubscriptionFormat,
    ) -> BoxFuture<'a, Url> {
        Box::pin(async move {
            fetch_airtcp_subscription_url(client, provider_url, cookies, format).await
        })
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

async fn fetch_airtcp_subscription_url(
    client: &Client,
    provider_url: &Url,
    cookies: &str,
    format: SubscriptionFormat,
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

    if let Some(url) = extract_subscription_url_from_text(provider_url, &user_html, format)? {
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
        if let Some(url) = extract_subscription_url_from_text(provider_url, &script, format)? {
            return Ok(url);
        }
    }

    bail!(
        "failed to find a {} subscription URL in the authenticated provider page",
        format.marker()
    )
}
