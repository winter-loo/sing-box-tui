use std::env;
use ratatui::style::{Color, Modifier, Style};

/// Terminal color support capability tier according to Figma Terminal Contract (04)
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ColorCapability {
    TrueColor,
    Ansi256,
    Ansi16,
    NoColor,
}

#[allow(dead_code)]
impl ColorCapability {
    pub(crate) fn detect() -> Self {
        if env::var("NO_COLOR").map(|v| !v.is_empty()).unwrap_or(false) {
            return Self::NoColor;
        }

        if let Ok(colorterm) = env::var("COLORTERM") {
            let val = colorterm.to_ascii_lowercase();
            if val == "truecolor" || val == "24bit" {
                return Self::TrueColor;
            }
        }

        if let Ok(term) = env::var("TERM") {
            let val = term.to_ascii_lowercase();
            if val.contains("256color") {
                return Self::Ansi256;
            }
            if val == "dumb" {
                return Self::NoColor;
            }
        }

        // On Windows with Windows Terminal / modern ConPTY, default to TrueColor
        #[cfg(windows)]
        {
            if env::var("WT_SESSION").is_ok() {
                return Self::TrueColor;
            }
        }

        Self::TrueColor
    }
}

/// Palette tokens based on Figma Foundations (826:20) and Terminal Contract (874:3)
#[allow(dead_code)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct Theme {
    pub(crate) capability: ColorCapability,
}

impl Default for Theme {
    fn default() -> Self {
        Self::new(ColorCapability::detect())
    }
}

#[allow(dead_code)]
impl Theme {
    pub(crate) fn detect() -> Self {
        Self::default()
    }

    pub(crate) fn new(capability: ColorCapability) -> Self {
        Self { capability }
    }

    // --- Background Colors ---

    pub(crate) fn bg_canvas(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(7, 12, 9),        // #070C09
            ColorCapability::Ansi256 => Color::Indexed(233),           // very dark gray
            ColorCapability::Ansi16 => Color::Black,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn bg_surface(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(7, 17, 13),       // #07110D
            ColorCapability::Ansi256 => Color::Indexed(234),
            ColorCapability::Ansi16 => Color::Black,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn bg_selected(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(14, 30, 23),      // #0E1E17
            ColorCapability::Ansi256 => Color::Indexed(23),            // #005F5F
            ColorCapability::Ansi16 => Color::DarkGray,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    // --- Foreground / Text Colors ---

    pub(crate) fn text_primary(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(220, 232, 223),   // #DCE8DF
            ColorCapability::Ansi256 => Color::Indexed(255),
            ColorCapability::Ansi16 => Color::White,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_secondary(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(169, 182, 174),   // #A9B6AE
            ColorCapability::Ansi256 => Color::Indexed(250),
            ColorCapability::Ansi16 => Color::Gray,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_muted(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(114, 128, 120),   // #728078
            ColorCapability::Ansi256 => Color::Indexed(243),           // #767676
            ColorCapability::Ansi16 => Color::DarkGray,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_accent(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(83, 216, 244),    // #53D8F4
            ColorCapability::Ansi256 => Color::Indexed(44),            // Cyan
            ColorCapability::Ansi16 => Color::Cyan,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_success(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(98, 230, 167),    // #62E6A7
            ColorCapability::Ansi256 => Color::Indexed(79),            // #5FD7AF
            ColorCapability::Ansi16 => Color::Green,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_warning(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(232, 212, 102),   // #E8D466
            ColorCapability::Ansi256 => Color::Indexed(221),           // #FFD75F
            ColorCapability::Ansi16 => Color::Yellow,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_error(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(255, 96, 107),    // #FF606B
            ColorCapability::Ansi256 => Color::Indexed(203),           // #FF5F5F
            ColorCapability::Ansi16 => Color::Red,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_transfer(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(0, 255, 209),     // #00FFD1
            ColorCapability::Ansi256 => Color::Indexed(49),
            ColorCapability::Ansi16 => Color::Cyan,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_status_active(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(79, 255, 184),    // #4FFFB8
            ColorCapability::Ansi256 => Color::Indexed(85),
            ColorCapability::Ansi16 => Color::Green,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn text_latency(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(214, 111, 230),   // #D66FE6
            ColorCapability::Ansi256 => Color::Indexed(170),
            ColorCapability::Ansi16 => Color::Magenta,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    // --- Route Interval Colors (alternating Cyan and Yellow per Figma 05) ---

    pub(crate) fn route_color(&self, interval_index: usize) -> Color {
        if interval_index % 2 == 0 {
            self.text_accent()
        } else {
            self.text_warning()
        }
    }

    // --- Border Colors ---

    pub(crate) fn border_default(&self) -> Color {
        match self.capability {
            ColorCapability::TrueColor => Color::Rgb(38, 50, 42),      // #26322A
            ColorCapability::Ansi256 => Color::Indexed(236),
            ColorCapability::Ansi16 => Color::DarkGray,
            ColorCapability::NoColor => Color::Reset,
        }
    }

    pub(crate) fn border_focus(&self) -> Color {
        self.text_accent()
    }

    // --- Composite Styles ---

    pub(crate) fn style_base(&self) -> Style {
        Style::default().bg(self.bg_canvas()).fg(self.text_primary())
    }

    pub(crate) fn style_focused_row(&self) -> Style {
        match self.capability {
            ColorCapability::NoColor => Style::default().add_modifier(Modifier::REVERSED),
            _ => Style::default()
                .bg(self.bg_selected())
                .fg(self.text_accent())
                .add_modifier(Modifier::BOLD),
        }
    }

    pub(crate) fn style_selected_marker(&self) -> Style {
        Style::default().fg(self.text_accent()).add_modifier(Modifier::BOLD)
    }

    pub(crate) fn style_muted(&self) -> Style {
        Style::default().fg(self.text_muted())
    }

    pub(crate) fn style_success(&self) -> Style {
        Style::default().fg(self.text_success())
    }

    pub(crate) fn style_warning(&self) -> Style {
        Style::default().fg(self.text_warning())
    }

    pub(crate) fn style_error(&self) -> Style {
        Style::default().fg(self.text_error())
    }

    pub(crate) fn style_breadcrumb(&self) -> Style {
        Style::default().fg(self.text_accent()).add_modifier(Modifier::BOLD)
    }

    pub(crate) fn style_footer_keys(&self) -> Style {
        Style::default().fg(self.text_accent())
    }

    pub(crate) fn style_footer_status(&self) -> Style {
        Style::default().fg(self.text_success()).add_modifier(Modifier::BOLD)
    }
}

#[cfg(test)]
mod tests {
    use super::{ColorCapability, Theme};
    use ratatui::style::Color;

    #[test]
    fn truecolor_palette_matches_live_figma_foundations() {
        let theme = Theme::new(ColorCapability::TrueColor);

        assert_eq!(theme.bg_canvas(), Color::Rgb(7, 12, 9));
        assert_eq!(theme.bg_surface(), Color::Rgb(7, 17, 13));
        assert_eq!(theme.bg_selected(), Color::Rgb(14, 30, 23));
        assert_eq!(theme.border_default(), Color::Rgb(38, 50, 42));
        assert_eq!(theme.border_focus(), Color::Rgb(83, 216, 244));
        assert_eq!(theme.text_primary(), Color::Rgb(220, 232, 223));
        assert_eq!(theme.text_secondary(), Color::Rgb(169, 182, 174));
        assert_eq!(theme.text_muted(), Color::Rgb(114, 128, 120));
        assert_eq!(theme.text_accent(), Color::Rgb(83, 216, 244));
        assert_eq!(theme.text_success(), Color::Rgb(98, 230, 167));
        assert_eq!(theme.text_warning(), Color::Rgb(232, 212, 102));
        assert_eq!(theme.text_error(), Color::Rgb(255, 96, 107));
        assert_eq!(theme.text_transfer(), Color::Rgb(0, 255, 209));
        assert_eq!(theme.text_status_active(), Color::Rgb(79, 255, 184));
        assert_eq!(theme.text_latency(), Color::Rgb(214, 111, 230));
    }

    #[test]
    fn no_color_palette_resets_every_semantic_token() {
        let theme = Theme::new(ColorCapability::NoColor);

        assert_eq!(theme.bg_canvas(), Color::Reset);
        assert_eq!(theme.bg_surface(), Color::Reset);
        assert_eq!(theme.bg_selected(), Color::Reset);
        assert_eq!(theme.border_default(), Color::Reset);
        assert_eq!(theme.border_focus(), Color::Reset);
        assert_eq!(theme.text_primary(), Color::Reset);
        assert_eq!(theme.text_secondary(), Color::Reset);
        assert_eq!(theme.text_muted(), Color::Reset);
        assert_eq!(theme.text_accent(), Color::Reset);
        assert_eq!(theme.text_success(), Color::Reset);
        assert_eq!(theme.text_warning(), Color::Reset);
        assert_eq!(theme.text_error(), Color::Reset);
        assert_eq!(theme.text_transfer(), Color::Reset);
        assert_eq!(theme.text_status_active(), Color::Reset);
        assert_eq!(theme.text_latency(), Color::Reset);
    }
}
