use std::time::Duration;

pub(crate) const DEFAULT_CONTROLLER: &str = "http://127.0.0.1:9992";
pub(crate) const DEFAULT_CONFIG_PATH: &str = "config.json";

pub(crate) fn default_clash_api_external_controller() -> &'static str {
    DEFAULT_CONTROLLER
        .strip_prefix("http://")
        .or_else(|| DEFAULT_CONTROLLER.strip_prefix("https://"))
        .unwrap_or(DEFAULT_CONTROLLER)
}
pub(crate) const DEFAULT_DELAY_TEST_URL: &str = "https://www.gstatic.com/generate_204";
pub(crate) const DEFAULT_VERIFICATION_TARGETS: &[(&str, &str)] = &[
    ("google", "https://www.google.com"),
    ("chatgpt", "https://chatgpt.com"),
    ("discord", "https://discord.com/api/v10/gateway"),
];
pub(crate) const DEFAULT_BENCHMARK_MAX_CONCURRENCY: usize = 16;
pub(crate) const DEFAULT_SELECTOR_TAG: &str = "手动选择";
pub(crate) const DEFAULT_AUTO_SELECTOR_TAG: &str = "自动选择";
pub(crate) const DEFAULT_AD_BLOCK_SELECTOR_TAG: &str = "广告路由";
pub(crate) const DEFAULT_DIRECT_TAG: &str = "国内直连";
pub(crate) const DEFAULT_BLOCK_TAG: &str = "屏蔽";
pub(crate) const DEFAULT_BYPASS_RULE_SET_TAG: &str = "sing-box-tui-bypass";
pub(crate) const DEFAULT_BYPASS_RULE_SET_PATH: &str = "sing-box-tui-bypass.json";
pub(crate) const DEFAULT_REMOTE_DNS_TAG: &str = "remote";
pub(crate) const DEFAULT_LOCAL_DNS_TAG: &str = "local";
pub(crate) const SELECTOR_TAG_ALIASES: &[&str] = &["手动选择", "select"];
pub(crate) const AUTO_SELECTOR_TAG_ALIASES: &[&str] = &["自动选择", "auto"];
pub(crate) const DIRECT_TAG_ALIASES: &[&str] = &["国内直连", "direct"];
pub(crate) const BLOCK_TAG_ALIASES: &[&str] = &["屏蔽", "block"];
pub(crate) const REFRESH_DEBOUNCE: Duration = Duration::from_millis(200);
pub(crate) const SINGLE_NODE_RETEST_DEBOUNCE: Duration = Duration::from_millis(800);
