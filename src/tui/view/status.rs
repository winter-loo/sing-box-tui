use super::*;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum StatusFooter {
    Status(String),
    Filter(String),
    Bypass(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StatusSnapshot {
    pub(crate) system_proxy_enabled: bool,
    pub(crate) tun_enabled: bool,
    pub(crate) selection_context: String,
    pub(crate) connections: String,
    pub(crate) subscription: String,
    pub(crate) sing_box: String,
    pub(crate) footer: StatusFooter,
}

pub(crate) fn node_order_badge(latency_sort_mode: bool) -> &'static str {
    if latency_sort_mode {
        "LATENCY ORDER"
    } else {
        "SELECTOR ORDER"
    }
}

pub(crate) fn pick_mode_badge(auto_select_enabled: bool) -> &'static str {
    if auto_select_enabled {
        "Auto"
    } else {
        "Manual"
    }
}

pub(crate) fn subscription_report_badge(report: &SubscriptionRefreshOutput) -> String {
    let providers = report
        .providers
        .iter()
        .map(|provider| {
            let warning = provider
                .warning
                .as_ref()
                .map(|warning| format!(" {}", truncate_for_width(warning, 24)))
                .unwrap_or_default();
            format!(
                "{}:{}:{} nodes{}",
                provider.provider, provider.status, provider.imported_nodes, warning
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    if report.config_updated {
        providers
    } else {
        format!("config unchanged; {providers}")
    }
}

pub(crate) fn format_duration_badge(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs >= 24 * 60 * 60 {
        let days = secs / (24 * 60 * 60);
        let hours = (secs % (24 * 60 * 60)) / 3600;
        if hours == 0 {
            format!("{days}d")
        } else {
            format!("{days}d{hours}h")
        }
    } else if secs >= 3600 {
        let hours = secs / 3600;
        let minutes = (secs % 3600) / 60;
        if minutes == 0 {
            format!("{hours}h")
        } else {
            format!("{hours}h{minutes}m")
        }
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

pub(crate) fn status_lines(status: &StatusSnapshot) -> Vec<Line<'static>> {
    vec![
        Line::from(vec![
            Span::styled("System Proxy: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if status.system_proxy_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Style::default().fg(if status.system_proxy_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::styled("Tun Mode: ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                if status.tun_enabled {
                    "enabled"
                } else {
                    "disabled"
                },
                Style::default().fg(if status.tun_enabled {
                    Color::Green
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw("  "),
            Span::raw(status.selection_context.clone()),
        ]),
        Line::from(status.connections.clone()),
        Line::from(status.subscription.clone()),
        Line::from(status.sing_box.clone()),
    ]
}

pub(crate) fn status_footer_line(footer: &StatusFooter) -> Line<'_> {
    let line = match footer {
        StatusFooter::Filter(input) => Line::from(vec![
            Span::styled("Filter: ", Style::default().fg(Color::Cyan)),
            Span::raw(input.as_str()),
            Span::styled(
                "  Enter apply  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        StatusFooter::Bypass(input) => Line::from(vec![
            Span::styled("Bypass: ", Style::default().fg(Color::Cyan)),
            Span::raw(input.as_str()),
            Span::styled(
                "  domains/IPs/CIDRs comma-separated  Enter save  Esc cancel",
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        StatusFooter::Status(status) => Line::from(status.as_str()),
    };
    line.patch_style(Style::default().fg(Color::DarkGray))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::subscriptions::ProviderRefreshSummary;

    #[test]
    fn subscription_and_duration_badges_are_compact() {
        let report = SubscriptionRefreshOutput {
            input_path: ".suburl".to_string(),
            cache_path: ".suburl.cache.json".to_string(),
            interval_days: 1,
            merged_config_path: "/usr/local/etc/sing-box/config.json".to_string(),
            backup_config_path: None,
            config_updated: true,
            node_history_reconciled: true,
            node_history_changed: true,
            node_quality_generation: Some(2),
            providers: vec![
                ProviderRefreshSummary {
                    provider: "宝贝云".to_string(),
                    subscription_url: "https://example.com/redacted".to_string(),
                    status: "fetched".to_string(),
                    imported_nodes: 67,
                    fetched_at_unix: 10,
                    warning: None,
                },
                ProviderRefreshSummary {
                    provider: "airtcp".to_string(),
                    subscription_url: "https://example.com/redacted".to_string(),
                    status: "cached".to_string(),
                    imported_nodes: 0,
                    fetched_at_unix: 10,
                    warning: Some("no mergeable nodes found".to_string()),
                },
            ],
        };

        let badge = subscription_report_badge(&report);
        assert!(badge.contains("宝贝云:fetched:67 nodes"));
        assert!(badge.contains("airtcp:cached:0 nodes"));
        assert!(badge.contains("no mergeable nodes found"));
        let unchanged_badge = subscription_report_badge(&SubscriptionRefreshOutput {
            config_updated: false,
            ..report.clone()
        });
        assert!(unchanged_badge.starts_with("config unchanged; "));
        assert_eq!(format_duration_badge(Duration::from_secs(42)), "42s");
        assert_eq!(format_duration_badge(Duration::from_secs(5 * 60)), "5m");
        assert_eq!(
            format_duration_badge(Duration::from_secs(2 * 3600 + 30 * 60)),
            "2h30m"
        );
        assert_eq!(
            format_duration_badge(Duration::from_secs(24 * 3600 + 3600)),
            "1d1h"
        );
    }
}
