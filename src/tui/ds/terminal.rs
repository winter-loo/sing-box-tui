use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Viewport adaptation mode according to Figma Terminal Contract (05 Viewport Behavior)
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ViewportMode {
    /// Canonical Standard layout (>= 120 cols x >= 30 rows)
    Standard,
    /// Compact layout (80-119 cols or 24-29 rows)
    Compact,
    /// Unsupported (< 80 cols or < 24 rows)
    Unsupported { width: u16, height: u16 },
}

#[allow(dead_code)]
impl ViewportMode {
    pub(crate) const MIN_COLS: u16 = 80;
    pub(crate) const MIN_ROWS: u16 = 24;
    pub(crate) const STANDARD_COLS: u16 = 120;
    pub(crate) const STANDARD_ROWS: u16 = 30;

    pub(crate) fn determine(area: Rect) -> Self {
        if area.width < Self::MIN_COLS || area.height < Self::MIN_ROWS {
            Self::Unsupported {
                width: area.width,
                height: area.height,
            }
        } else if area.width >= Self::STANDARD_COLS && area.height >= Self::STANDARD_ROWS {
            Self::Standard
        } else {
            Self::Compact
        }
    }

    pub(crate) fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported { .. })
    }
}

/// Truncate a string by visual width (wcwidth), strictly respecting Grapheme Cluster boundaries.
/// Never breaks multi-byte UTF-8 sequences or compound emojis.
/// Adds `suffix` (e.g. "...") if truncation occurred.
#[allow(dead_code)]
pub(crate) fn truncate_cjk_graphemes(s: &str, max_width: usize, suffix: &str) -> String {
    let s_width = UnicodeWidthStr::width(s);
    if s_width <= max_width {
        return s.to_string();
    }

    let suffix_width = UnicodeWidthStr::width(suffix);
    if max_width <= suffix_width {
        return suffix.chars().take(max_width).collect();
    }

    let target_width = max_width - suffix_width;
    let mut current_width = 0;
    let mut result = String::new();

    for grapheme in UnicodeSegmentation::graphemes(s, true) {
        let g_width = UnicodeWidthStr::width(grapheme);
        if current_width + g_width > target_width {
            break;
        }
        result.push_str(grapheme);
        current_width += g_width;
    }

    result.push_str(suffix);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_viewport_mode_thresholds() {
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 120, 30)),
            ViewportMode::Standard
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 140, 40)),
            ViewportMode::Standard
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 80, 24)),
            ViewportMode::Compact
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 119, 30)),
            ViewportMode::Compact
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 120, 29)),
            ViewportMode::Compact
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 79, 24)),
            ViewportMode::Unsupported { width: 79, height: 24 }
        );
        assert_eq!(
            ViewportMode::determine(Rect::new(0, 0, 80, 23)),
            ViewportMode::Unsupported { width: 80, height: 23 }
        );
    }

    #[test]
    fn test_truncate_cjk_graphemes() {
        let ascii = "Tokyo-Edge-01";
        assert_eq!(truncate_cjk_graphemes(ascii, 20, "..."), "Tokyo-Edge-01");
        assert_eq!(truncate_cjk_graphemes(ascii, 10, "..."), "Tokyo-E...");

        // CJK characters take 2 columns each
        let cjk = "东京直连专线节点"; // 8 chars = 16 width
        assert_eq!(truncate_cjk_graphemes(cjk, 20, "..."), "东京直连专线节点");
        // max 11: suffix is 3 ("..."), available = 8 -> 4 CJK chars (width 8) + "..."
        assert_eq!(truncate_cjk_graphemes(cjk, 11, "..."), "东京直连...");
        // max 10: suffix is 3, available = 7 -> only 3 CJK chars (width 6) + "..."
        assert_eq!(truncate_cjk_graphemes(cjk, 10, "..."), "东京直...");
    }
}
