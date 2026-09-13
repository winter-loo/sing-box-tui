//! Standalone fixture renderer. No production state, network requests or input handlers.
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    symbols::Marker,
    widgets::{Axis, Block, Borders, Chart, Dataset, GraphType, Paragraph},
};
use std::{fs, io, path::Path};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

const BG: Color = Color::Rgb(7, 17, 14);
const FG: Color = Color::Rgb(195, 207, 200);
const MUTED: Color = Color::Rgb(114, 128, 120);
const A: Color = Color::Rgb(77, 214, 239);
const B: Color = Color::Rgb(232, 212, 102);
const PINK: Color = Color::Rgb(229, 137, 245);
const GREEN: Color = Color::Rgb(98, 230, 167);

fn fit(s: &str, width: u16) -> String {
    if s.width() <= usize::from(width) {
        return s.into();
    }
    let budget = usize::from(width).saturating_sub(3);
    let mut out = String::new();
    for g in s.graphemes(true) {
        if out.width() + g.width() > budget {
            break;
        }
        out.push_str(g);
    }
    out.push_str(&"..."[..usize::from(width).min(3)]);
    out
}
fn label(f: &mut Frame, x: u16, y: u16, w: u16, s: &str, color: Color) {
    f.render_widget(
        Paragraph::new(fit(s, w)).style(Style::default().fg(color)),
        Rect::new(x, y, w, 1),
    );
}
fn panel(f: &mut Frame, r: Rect, title: &str) {
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(MUTED))
            .title(title),
        r,
    );
}
fn plot(
    f: &mut Frame,
    r: Rect,
    points: &[Vec<(f64, f64)>],
    colors: &[Color],
    kinds: &[GraphType],
    max: f64,
) {
    let sets = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            Dataset::default()
                .data(p)
                .graph_type(kinds[i])
                .marker(if kinds[i] == GraphType::Scatter {
                    Marker::Dot
                } else {
                    Marker::Braille
                })
                .style(Style::default().fg(colors[i]))
        })
        .collect::<Vec<_>>();
    f.render_widget(
        Chart::new(sets)
            .x_axis(Axis::default().bounds([0., 30.]))
            .y_axis(Axis::default().bounds([0., max]))
            .legend_position(None),
        r,
    );
}
fn aggregate(f: &mut Frame, r: Rect) {
    panel(f, r, " 代理历史 · 30 分钟 ");
    let x = r.x + 1;
    let w = r.width - 2;
    let top = r.y + 1;
    let ph = (r.height - 12) / 2;
    let px = x + 8;
    let pw = w - 8;
    let py = top + 3;
    let ty = py + ph + 2;
    for (minute, name, color) in [(0, "东京", A), (10, "香港", B), (20, "东京", A)] {
        let bx = px + ((f64::from(pw - 1) * f64::from(minute) / 30.).round() as u16);
        label(f, bx, top + 1, (px + pw - bx).min(14), name, color);
    }
    label(f, x, top + 2, w, "延迟 · ms", FG);
    label(f, x, py, 7, "120", MUTED);
    label(f, x, py + ph - 1, 7, "0", MUTED);
    label(f, x, py + ph, w, "吞吐 ≈ · MiB/s", FG);
    label(f, x, py + ph + 1, w, "↓ 线   ↑ 点", FG);
    label(f, x, ty, 7, "10", MUTED);
    label(f, x, ty + ph - 1, 7, "0", MUTED);
    let intervals = [(0, 10, A), (10, 15, B), (18, 20, B), (20, 31, A)];
    let mut latency = Vec::new();
    let mut traffic = Vec::new();
    let mut colors = Vec::new();
    for (start, end, color) in intervals {
        latency.push(
            (start..end)
                .map(|t| (f64::from(t), 42. + (f64::from(t) * 0.6).sin() * 18.))
                .collect(),
        );
        traffic.push(
            (start..end)
                .map(|t| (f64::from(t), 5. + (f64::from(t) * 0.4).sin() * 2.))
                .collect(),
        );
        traffic.push(
            (start..end)
                .map(|t| (f64::from(t), 1.5 + (f64::from(t) * 0.7).sin() * 0.7))
                .collect(),
        );
        colors.push(color);
    }
    plot(
        f,
        Rect::new(px, py, pw, ph),
        &latency,
        &colors,
        &[GraphType::Line; 4],
        120.,
    );
    let traffic_colors = colors.iter().flat_map(|c| [*c, *c]).collect::<Vec<_>>();
    plot(
        f,
        Rect::new(px, ty, pw, ph),
        &traffic,
        &traffic_colors,
        &[
            GraphType::Line,
            GraphType::Scatter,
            GraphType::Line,
            GraphType::Scatter,
            GraphType::Line,
            GraphType::Scatter,
            GraphType::Line,
            GraphType::Scatter,
        ],
        10.,
    );
    let axis = ty + ph;
    label(f, px, axis, pw, &"─".repeat(usize::from(pw)), MUTED);
    for (fraction, text) in [(0., "-30m"), (0.5, "-15m"), (1., "现在")] {
        let offset = ((f64::from(pw - 1) * fraction).round() as u16).min(pw - text.width() as u16);
        label(f, px + offset, axis + 1, text.width() as u16, text, MUTED);
    }
}
fn node(f: &mut Frame, r: Rect) {
    panel(f, r, " 节点质量 ");
    let x = r.x + 1;
    let w = r.width - 2;
    let y = r.y + 1;
    // Separate measurements retain their own sample ages; history stays sparse.
    label(f, x, y, w - 8, "延迟 28 ms", PINK);
    label(f, x + w - 7, y, 7, "刚测", MUTED);
    label(f, x, y + 1, 4, "120", MUTED);
    label(f, x, y + 3, 4, "0", MUTED);
    plot(
        f,
        Rect::new(x + 5, y + 1, w - 5, 3),
        &[vec![
            (2., 45.),
            (9., 55.),
            (21., 35.),
            (29., 50.),
            (30., 28.),
        ]],
        &[PINK],
        &[GraphType::Scatter],
        120.,
    );
    label(f, x, y + 4, w - 8, "实测 8.0 MiB/s", GREEN);
    label(f, x + w - 7, y + 4, 7, "2分钟前", MUTED);
    label(f, x, y + 5, 4, "10", MUTED);
    label(f, x, y + 7, 4, "0", MUTED);
    plot(
        f,
        Rect::new(x + 5, y + 5, w - 5, 3),
        &[vec![(3., 7.), (13., 5.), (28., 8.)]],
        &[GREEN],
        &[GraphType::Scatter],
        10.,
    );
    label(f, x + 5, y + 8, 4, "-30m", MUTED);
    label(f, x + w - 4, y + 8, 4, "现在", MUTED);
    label(f, x, y + 9, w, "仅显示已有采样", MUTED);
}

fn connections(f: &mut Frame, r: Rect) {
    panel(f, r, " 活动连接 · 12 ");
    let x = r.x + 1;
    let w = r.width - 2;
    label(f, x, r.y + 1, w, "目标", MUTED);
    label(f, x + w - 6, r.y + 1, 6, "MiB/s", MUTED);
    for (i, name) in [
        "github.com",
        "东京服务.example",
        "chat.openai.com",
        "registry.npmjs.org",
        "objects.githubusercontent.com",
    ]
    .iter()
    .enumerate()
    {
        label(f, x, r.y + 2 + i as u16, w - 7, name, FG);
        label(f, x + w - 6, r.y + 2 + i as u16, 6, "  0.42", FG);
    }
    label(f, x, r.y + 10, w, "另 7 条", MUTED);
}
fn render(f: &mut Frame) {
    let r = f.area();
    f.render_widget(Block::default().style(Style::default().bg(BG).fg(FG)), r);
    if r.width < 80 || r.height < 24 {
        label(f, 0, 0, r.width, "请将终端调整至至少 80×24", FG);
        if r.height > 1 {
            label(f, 0, 1, r.width, "q 退出   ? 帮助", A);
        }
        return;
    }
    panel(f, Rect::new(0, 0, r.width, 4), " 监控 ");
    label(f, 1, 1, r.width - 2, "网络 / 宝贝云 / 东京专线", FG);
    label(
        f,
        1,
        2,
        r.width - 2,
        "28 ms     ↓ 3.9 MiB/s     ↑ 2.1 MiB/s",
        GREEN,
    );
    let full = r.width >= 120 && r.height >= 30;
    let with_node = r.width >= 96 && r.height >= 30;
    let left = if full {
        38
    } else if with_node {
        30
    } else {
        0
    };
    if with_node {
        node(f, Rect::new(0, 4, left, 12));
    }
    if full {
        connections(f, Rect::new(0, 16, left, r.height - 18));
    }
    aggregate(f, Rect::new(left, 4, r.width - left, r.height - 6));
    label(
        f,
        0,
        r.height - 2,
        r.width,
        "Ctrl+K 导航   c 连接   i 节点   o 设置   ? 帮助   q 退出",
        A,
    );
    label(
        f,
        0,
        r.height - 1,
        r.width,
        "历史有缺测    探测流量 —",
        MUTED,
    );
}
fn rgb(c: Color) -> (u8, u8, u8) {
    match c {
        Color::Rgb(r, g, b) => (r, g, b),
        _ => (195, 207, 200),
    }
}
fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
fn export(b: &Buffer, path: &Path) -> io::Result<()> {
    let mut text = String::new();
    let mut ansi = String::new();
    let mut svg = format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\"><rect width=\"100%\" height=\"100%\" fill=\"#07110e\"/><g font-family=\"Cascadia Mono,Consolas,monospace\" font-size=\"14\">",
        b.area.width * 9,
        b.area.height * 18,
        b.area.width * 9,
        b.area.height * 18
    );
    for y in 0..b.area.height {
        let mut skip = 0;
        for x in 0..b.area.width {
            if skip > 0 {
                skip -= 1;
                continue;
            }
            let c = &b[(x, y)];
            let s = c.symbol();
            let width = s.width().max(1);
            skip = width - 1;
            text.push_str(s);
            let (r, g, bl) = rgb(c.fg);
            ansi.push_str(&format!("\x1b[38;2;{r};{g};{bl}m{s}"));
            if s != " " {
                svg.push_str(&format!("<text x=\"{}\" y=\"{}\" fill=\"#{r:02x}{g:02x}{bl:02x}\" textLength=\"{}\" lengthAdjust=\"spacingAndGlyphs\">{}</text>",x*9,y*18+14,width*9,xml(s)));
            }
        }
        text.push('\n');
        ansi.push_str("\x1b[0m\n");
    }
    svg.push_str("</g></svg>");
    fs::write(path.with_extension("txt"), text)?;
    fs::write(path.with_extension("ansi"), ansi)?;
    fs::write(path.with_extension("svg"), svg)
}
fn main() -> io::Result<()> {
    let out = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "artifacts/idle-dashboard-layout".into());
    fs::create_dir_all(&out)?;
    for (w, h) in [
        (120, 30),
        (119, 30),
        (96, 30),
        (95, 30),
        (120, 29),
        (80, 24),
        (79, 24),
        (80, 23),
        (20, 4),
    ] {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(render).unwrap();
        let b = t.backend().buffer();
        export(b, &Path::new(&out).join(format!("idle-{w}x{h}")))?;
        println!("rendered {w}x{h}: {} cells", b.content.len());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn buffer(w: u16, h: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(render).unwrap();
        terminal.backend().buffer().clone()
    }
    fn text(b: &Buffer) -> String {
        let mut out = String::new();
        for y in 0..b.area.height {
            let mut x = 0;
            while x < b.area.width {
                let s = b[(x, y)].symbol();
                out.push_str(s);
                x += s.width().max(1) as u16;
            }
            out.push('\n');
        }
        out
    }
    #[test]
    fn compact_hides_connections_before_node_and_honors_minimum() {
        for (w, h, node, connections) in [
            (120, 30, true, true),
            (119, 30, true, false),
            (96, 30, true, false),
            (95, 30, false, false),
            (120, 29, false, false),
            (80, 24, false, false),
        ] {
            let b = buffer(w, h);
            let t = text(&b);
            assert_eq!(t.contains("节点质量"), node);
            assert_eq!(t.contains("活动连接"), connections);
            assert!(t.contains("c 连接") && t.contains("i 节点"));
            assert!(t.contains("延迟 · ms"));
        }
        for (w, h) in [(79, 24), (80, 23)] {
            let t = text(&buffer(w, h));
            assert!(t.contains("请将终端调整至至少 80×24"));
            assert!(t.contains("q 退出") && t.contains("? 帮助"));
        }
    }
    #[test]
    fn both_charts_share_boundaries_and_leave_missing_window_blank() {
        let b = buffer(120, 30);
        // Labels and both datasets use the same time projection; returning to Tokyo
        // creates a new cyan segment rather than reconnecting the old interval.
        for (x, color) in [(47, A), (71, B), (94, A)] {
            assert_eq!(b[(x, 6)].fg, color);
            for mut rows in [8..14, 16..22] {
                assert!(
                    rows.clone().any(|y| (x..x + 8).any(|cx| {
                        let cell = &b[(cx, y)];
                        cell.symbol() != " " && cell.fg == color
                    })),
                    "missing interval color at {x}"
                );
                assert!(rows.all(|y| (47..119).all(|cx| b[(cx, y)].symbol() != "|")));
            }
        }
        // The interior of the -15m..-12m missing interval, excluding endpoint cells.
        for x in 84..88 {
            for y in (8..14).chain(16..22) {
                assert_eq!(b[(x, y)].symbol(), " ", "gap bridged at {x},{y}");
            }
        }
        assert!(b.content.iter().any(|c| c.symbol() == "•" && c.fg == A));
        assert!(b.content.iter().any(|c| c.symbol() == "•" && c.fg == B));
    }
    #[test]
    fn node_quality_exposes_values_ages_and_scaled_sparse_history() {
        for width in [120, 96] {
            let b = buffer(width, 30);
            let t = text(&b);
            for label in ["28 ms", "8.0 MiB/s", "刚测", "2分钟前", "仅显示已有采样"] {
                assert!(t.contains(label), "missing node label {label} at {width}");
            }
            assert_eq!(b[(1, 6)].symbol(), "1");
            assert_eq!(b[(1, 8)].symbol(), "0");
            assert_eq!(b[(1, 10)].symbol(), "1");
            assert_eq!(b[(1, 12)].symbol(), "0");
            for (rows, color, count) in [(6..9, PINK, 5), (10..13, GREEN, 3)] {
                let right = if width == 120 { 37 } else { 29 };
                let samples = rows
                    .flat_map(|y| (6..right).map(move |x| (x, y)))
                    .filter(|&(x, y)| b[(x, y)].symbol() == "•" && b[(x, y)].fg == color)
                    .count();
                assert_eq!(samples, count);
            }
        }
        assert!(!text(&buffer(80, 24)).contains("8.0 MiB/s"));
    }
    #[test]
    fn labels_keep_graphemes_and_reserved_metrics() {
        assert_eq!(fit("东京专线", 7), "东京...");
        assert_eq!(fit("e\u{301}abc", 3), "...");
        assert_eq!(fit("👩‍💻abcdef", 5), "👩‍💻...");
        let b = buffer(120, 30);
        for y in 18..23 {
            assert_eq!(b[(36, y)].symbol(), "2");
        }
    }
}
