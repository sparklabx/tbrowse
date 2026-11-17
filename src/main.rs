//! tbrowse: a terminal web browser.
//! Obscura (Rust + V8, ~60 MB RAM) does HTML/CSS/JS, layout and paint over CDP.
//! This program only shows its screenshots in the terminal (kitty / iTerm2 / sixel /
//! half-block fallback via viuer) and forwards mouse + keyboard back as CDP input.

use base64::Engine as _;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::{cursor, execute, queue, terminal};
use serde_json::{json, Value};
use std::io::{stdout, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tungstenite::{stream::MaybeTlsStream, Message, WebSocket};

const HELP: &str = "^L url  ^B back  ^R reload  ^Q quit";

struct Cdp {
    ws: WebSocket<MaybeTlsStream<TcpStream>>,
    id: u64,
    session: Option<String>,
}

impl Cdp {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.id += 1;
        let mut msg = json!({ "id": self.id, "method": method, "params": params });
        if let Some(s) = &self.session {
            msg["sessionId"] = json!(s);
        }
        self.ws.send(Message::text(msg.to_string())).map_err(|e| e.to_string())?;
        loop {
            let Message::Text(t) = self.ws.read().map_err(|e| e.to_string())? else { continue };
            let v: Value = serde_json::from_str(t.as_str()).map_err(|e| e.to_string())?;
            if v["id"].as_u64() == Some(self.id) {
                return match v.get("error") {
                    Some(e) => Err(e.to_string()),
                    None => Ok(v["result"].clone()),
                };
            } // anything else is an event we don't use
        }
    }

    fn eval(&mut self, js: &str) -> Value {
        self.call("Runtime.evaluate", json!({ "expression": js, "returnByValue": true }))
            .map(|r| r["result"]["value"].clone())
            .unwrap_or(Value::Null)
    }

    fn key(&mut self, key: &str, vk: u32) {
        for t in ["keyDown", "keyUp"] {
            let _ = self.call("Input.dispatchKeyEvent", json!({ "type": t, "key": key, "code": key, "windowsVirtualKeyCode": vk }));
        }
    }
}

fn connect(port: u16) -> Result<Cdp, String> {
    let url = format!("ws://127.0.0.1:{port}/devtools/browser");
    let start = Instant::now();
    let ws = loop {
        match tungstenite::connect(url.as_str()) {
            Ok((ws, _)) => break ws,
            Err(e) if start.elapsed() > Duration::from_secs(15) => return Err(e.to_string()),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    };
    let mut cdp = Cdp { ws, id: 0, session: None };
    let t = cdp.call("Target.createTarget", json!({ "url": "about:blank" }))?;
    let a = cdp.call("Target.attachToTarget", json!({ "targetId": t["targetId"], "flatten": true }))?;
    cdp.session = a["sessionId"].as_str().map(String::from);
    cdp.call("Page.enable", json!({}))?;
    Ok(cdp)
}

/// Kills the obscura server when we exit (also on panic).
struct Obscura(Child);
impl Drop for Obscura {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn obscura_bin() -> std::path::PathBuf {
    if let Some(p) = std::env::var_os("OBSCURA") {
        return p.into();
    }
    let name = if cfg!(windows) { "obscura.exe" } else { "obscura" };
    std::env::current_exe()
        .ok()
        .and_then(|e| Some(e.parent()?.join(name)))
        .filter(|p| p.exists())
        .unwrap_or_else(|| name.into()) // fall back to PATH
}

/// TBROWSE_BLOCKS=1 forces text mode (e.g. VS Code with images turned off).
fn blocks_only() -> bool {
    std::env::var_os("TBROWSE_BLOCKS").is_some()
}

fn clear_kitty() {
    if viuer::get_kitty_support() != viuer::KittySupport::None {
        print!("\x1b_Ga=d,d=A\x1b\\"); // kitty keeps old images unless told to delete them
    }
}

/// Restores the terminal on exit (also on panic).
struct Screen;
impl Screen {
    fn enter() -> std::io::Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(stdout(), terminal::EnterAlternateScreen, event::EnableMouseCapture, cursor::Hide)?;
        Ok(Screen)
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        clear_kitty();
        let _ = execute!(stdout(), event::DisableMouseCapture, cursor::Show, terminal::LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[derive(Clone, Copy)]
struct View {
    cols: u16,
    page_rows: u16, // the last terminal row is the status bar
    css_w: u32,
    css_h: u32,
    dpr: u32,
    sharp: bool, // terminal shows real images; false = text mode (terminal glyphs + half blocks)
}

fn view() -> View {
    let (cols, rows) = terminal::size().unwrap_or((100, 30));
    let (cols, rows) = (cols.max(10), rows.max(2));
    let page_rows = rows - 1;
    let sharp = !blocks_only()
        && (viuer::is_iterm_supported() || viuer::get_kitty_support() != viuer::KittySupport::None || viuer::is_sixel_supported());
    let (css_w, css_h, dpr) = match terminal::window_size() {
        // Real pixel size (unix). Retina terminals report ~2x pixels per cell.
        Ok(w) if w.width > 0 && w.height > 0 => {
            let (pw, ph) = (w.width as u32, w.height as u32 * page_rows as u32 / rows as u32);
            let dpr = if pw / cols as u32 >= 14 { 2 } else { 1 };
            (pw / dpr, ph / dpr, dpr)
        }
        // Unknown (Windows, VS Code): lay out as 8x16 cells; with an image protocol shoot 2x so it isn't upscaled blurry.
        _ if sharp => (cols as u32 * 8, page_rows as u32 * 16, 2),
        // text mode: 7x14 css px cells ~ one 14-16px glyph per cell, so text runs rarely overflow their box
        _ => (cols as u32 * 7, page_rows as u32 * 14, 1),
    };
    View { cols, page_rows, css_w, css_h, dpr, sharp }
}

/// Terminal cell -> page CSS pixel, using the size viuer actually printed (it keeps aspect ratio).
fn to_css(v: &View, printed: (u32, u32), col: u16, row: u16) -> (f64, f64) {
    let (w, h) = (printed.0.max(1) as f64, printed.1.max(1) as f64);
    ((col as f64 + 0.5) * v.css_w as f64 / w, (row as f64 + 0.5) * v.css_h as f64 / h)
}

fn to_url(s: &str) -> String {
    let s = s.trim();
    if s.contains("://") || s.starts_with("about:") {
        s.into()
    } else if s.contains('.') && !s.contains(' ') {
        format!("https://{s}")
    } else {
        let q: String = s
            .bytes()
            .map(|b| if b.is_ascii_alphanumeric() { (b as char).to_string() } else { format!("%{b:02X}") })
            .collect();
        format!("https://html.duckduckgo.com/html/?q={q}")
    }
}

fn status(v: &View, text: &str) {
    let w = v.cols as usize - 1; // never touch the last cell: some terminals scroll
    let line: String = text.chars().take(w).collect();
    let mut o = stdout();
    let _ = queue!(
        o,
        cursor::MoveTo(0, v.page_rows),
        SetAttribute(Attribute::Reverse),
        Print(format!("{line:<w$}")),
        SetAttribute(Attribute::Reset)
    );
    let _ = o.flush();
}

fn prompt(v: &View, label: &str) -> Option<String> {
    let mut s = String::new();
    loop {
        status(v, &format!("{label}{s}_"));
        let Ok(Event::Key(k)) = event::read() else { continue };
        if k.kind != KeyEventKind::Press {
            continue;
        }
        match k.code {
            KeyCode::Enter if !s.trim().is_empty() => return Some(s),
            KeyCode::Esc => return None,
            KeyCode::Backspace => {
                s.pop();
            }
            KeyCode::Char(c) => s.push(c),
            _ => {}
        }
    }
}

struct Tab {
    cdp: Cdp,
    v: View,
    last: Vec<u8>,
    printed: (u32, u32),
    cells: Vec<Out>, // text mode: what's on screen now, for diffing
    scroll_y: f64,   // text mode: page scrollY those cells show
    truecolor: bool,
}

impl Tab {
    fn resize(&mut self) {
        self.v = view();
        let v = self.v;
        let _ = self.cdp.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": v.css_w, "height": v.css_h, "deviceScaleFactor": v.dpr, "mobile": false }),
        );
        self.last.clear();
        self.cells.clear();
        clear_kitty();
        let _ = execute!(stdout(), terminal::Clear(terminal::ClearType::All));
    }

    fn go(&mut self, url: &str) {
        status(&self.v, &format!(" loading {url} ..."));
        // shortcut: Page.navigate blocks until load, so the UI freezes on slow pages; move CDP to a thread if that hurts
        if let Err(e) = self.cdp.call("Page.navigate", json!({ "url": url })) {
            status(&self.v, &format!(" error: {e}"));
            std::thread::sleep(Duration::from_secs(2));
        }
        self.draw();
    }

    fn draw(&mut self) {
        let Ok(r) = self.cdp.call("Page.captureScreenshot", json!({ "format": "png" })) else { return };
        let Some(png) = r["data"].as_str().and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok()) else {
            return;
        };
        if png != self.last {
            // only repaint when pixels changed: no flicker while idle
            if let Ok(img) = image::load_from_memory(&png) {
                let (cols, rows) = (self.v.cols as u32, self.v.page_rows as u32);
                if self.v.sharp {
                    clear_kitty();
                    let cfg = viuer::Config { x: 0, y: 0, width: Some(cols), height: Some(rows), ..Default::default() };
                    if let Ok(p) = viuer::print(&img, &cfg) {
                        self.printed = p;
                    }
                } else {
                    let (cw, ch) = (self.v.css_w as f64 / cols as f64, self.v.css_h as f64 / rows as f64);
                    let g = self.cdp.eval(&format!("({GRID_JS})({cols}, {rows}, {cw}, {ch})"));
                    let runs: Vec<Run> = g["runs"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|r| {
                            Some(Run {
                                row: r[0].as_u64()? as usize,
                                col: r[1].as_u64()? as usize,
                                text: r[2].as_str()?.to_string(),
                                color: parse_css_color(r[3].as_str().unwrap_or("")),
                            })
                        })
                        .collect();
                    let bgs: Vec<Option<Rgb>> =
                        g["bgs"].as_array().into_iter().flatten().map(|c| parse_css_color(c.as_str().unwrap_or(""))).collect();
                    let texty: Vec<BoxBg> = g["texty"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|i| match i.as_u64().unwrap_or(0) as usize {
                            0 => None,
                            k => Some(bgs.get(k - 1).copied().flatten()),
                        })
                        .collect();
                    let cur = render_text_mode(&img.to_rgb8(), cols as usize, rows as usize, &runs, &texty);
                    // whole-row scroll since the last frame -> let the terminal move those rows
                    let sy = g["scrollY"].as_f64().unwrap_or(0.0);
                    let k = (sy - self.scroll_y) / ch;
                    let shift = if (k - k.round()).abs() < 0.01 { k.round() as i32 } else { i32::MAX };
                    self.scroll_y = sy;
                    let out = emit(&mut self.cells, &cur, cols as usize, rows as usize, shift, self.truecolor);
                    let mut o = stdout();
                    let _ = o.write_all(out.as_bytes());
                    let _ = o.flush();
                    self.printed = (cols, rows);
                }
            }
            self.last = png;
        }
        let info = self.cdp.eval("document.title + '  |  ' + location.href");
        status(&self.v, &format!(" {}    {HELP}", info.as_str().unwrap_or("")));
    }

    /// Scroll by whole terminal rows (so text mode can shift the screen instead of repainting it).
    fn scroll_rows(&mut self, rows: i32, x: f64, y: f64) {
        let ch = self.v.css_h as f64 / self.v.page_rows.max(1) as f64;
        let _ = self.cdp.call(
            "Input.dispatchMouseEvent",
            json!({ "type": "mouseWheel", "x": x, "y": y, "deltaX": 0, "deltaY": rows as f64 * ch }),
        );
    }
}

/// Text mode: where every visible line of text sits on the cell grid.
/// Measuring wraps each text node in a temporary inline element (a bare text node reports its parent's
/// whole box; the wrapper gives tight per-line rects), all at once, then unwraps. The result is cached in
/// document coordinates and only re-measured when the DOM, viewport or page height changes, so a scroll
/// frame just shifts the cached lines by scrollY.
/// Returns { runs: [[row, col, text, cssColor]], texty: [cell -> 1-based index into bgs, 0 = none], bgs, scrollY }.
const GRID_JS: &str = r#"(cols, rows, cw, ch) => {
  const T = window.__tbrowse || (window.__tbrowse = { mut: 0 });
  if (!T.mo) {
    T.mo = new MutationObserver(() => T.mut++);
    // not attributes: carousels/ads toggle classes all the time, which would re-measure every frame
    T.mo.observe(document, { subtree: true, childList: true, characterData: true });
  }
  const key = [T.mut, innerWidth, innerHeight, document.documentElement.scrollHeight].join();
  // measure a band around the viewport (one screen above, two below); reuse it while scrolling inside it
  if (T.key !== key || scrollY < T.band0 || scrollY + innerHeight > T.band1) {
    T.key = key;
    T.lines = [];
    T.band0 = scrollY - innerHeight;
    T.band1 = scrollY + 2 * innerHeight;
    const skip = /^(SCRIPT|STYLE|NOSCRIPT|TEXTAREA|TITLE|TEMPLATE|OPTION)$/, nodes = [];
    const walk = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let n; (n = walk.nextNode()); ) {
      const p = n.parentElement;
      if (!n.data.trim() || !p || skip.test(p.tagName)) continue;
      const b = p.getBoundingClientRect(); // cheap pre-filter before the (costly) wrap + measure
      if ((!b.width && !b.height) || b.bottom + scrollY < T.band0 || b.top + scrollY > T.band1) continue;
      nodes.push(n);
    }
    const style = new Map(), info = el => { // per element: color, hidden, position:fixed, solid background
      if (!el) return { bg: 'rgb(255, 255, 255)', fixed: false, hidden: false };
      if (style.has(el)) return style.get(el);
      const cs = getComputedStyle(el), up = info(el.parentElement);
      const own = cs.backgroundImage && cs.backgroundImage !== 'none' ? null // gradient/image: use pixels
        : /rgba\(.*,\s*0\)$|transparent/.test(cs.backgroundColor) ? undefined : cs.backgroundColor;
      const v = { color: cs.color, bg: own === undefined ? up.bg : own, fixed: up.fixed || cs.position === 'fixed',
                  hidden: up.hidden || cs.visibility === 'hidden' || cs.opacity === '0' || cs.display === 'none' };
      style.set(el, v);
      return v;
    };
    // shortcut: while wrapped, structural selectors (:first-child...) can see the wrapper and nudge layout there
    const tags = nodes.map(n => {
      const t = document.createElement('tb-m');
      t.setAttribute('style', 'display:inline'); // Obscura lays unknown elements out as blocks
      n.replaceWith(t);
      t.appendChild(n);
      return t;
    });
    const sx = scrollX, sy = scrollY;
    tags.forEach((t, j) => {
      const I = info(t.parentElement);
      if (I.hidden) return;
      const rs = [...t.getClientRects()].filter(r => r.width > 0 && r.height > 0);
      if (!rs.length) return;
      // split the text over its line boxes in proportion to their widths, snapped to the nearest space
      const text = nodes[j].data.replace(/\s+/g, ' '), total = rs.reduce((a, r) => a + r.width, 0);
      let from = 0, acc = 0;
      rs.forEach((r, i) => {
        acc += r.width;
        let to = text.length;
        if (i < rs.length - 1) {
          to = Math.round(text.length * acc / total);
          let best = -1;
          for (let k = Math.max(from + 1, to - 8); k <= Math.min(text.length - 1, to + 8); k++)
            if (text[k] === ' ' && (best < 0 || Math.abs(k - to) < Math.abs(best - to))) best = k;
          if (best >= 0) to = best + 1;
          to = Math.max(from, Math.min(text.length, to));
        }
        const ox = I.fixed ? 0 : sx, oy = I.fixed ? 0 : sy;
        T.lines.push([r.left + ox, r.top + oy, r.width, r.height, text.slice(from, to).trim(), I.color, I.bg, I.fixed]);
        from = to;
      });
    });
    tags.forEach((t, j) => t.replaceWith(nodes[j]));
    T.mo.takeRecords(); // our own wrapping is not a page change
  }
  const runs = [], bgs = [], texty = new Array(cols * rows).fill(0), sx = scrollX, sy = scrollY;
  for (const [x, y, w, h, t, color, bg, fixed] of T.lines) {
    const left = fixed ? x : x - sx, top = fixed ? y : y - sy;
    if (top + h <= 0 || top >= rows * ch || left + w <= 0 || left >= cols * cw) continue;
    const r = Math.floor((top + h / 2) / ch), c0 = Math.max(0, Math.floor(left / cw)), c1 = Math.min(cols, Math.ceil((left + w) / cw));
    // big type spans more cells than it has chars: centre the run on the line's pixel extent
    if (t && r >= 0 && r < rows) runs.push([r, c0 + Math.max(0, Math.floor((c1 - c0 - t.length) / 2)), t, color]);
    // cells under a text line get a flat background, so glyph pixels don't smudge next to the real text
    // (and not the cell's dominant pixel color: in big headings that's the glyph color)
    bgs.push(bg);
    for (let rr = Math.max(0, Math.floor(top / ch)); rr < Math.min(rows, Math.ceil((top + h) / ch)); rr++)
      for (let c = c0; c < c1; c++) texty[rr * cols + c] = bgs.length;
  }
  return { runs, texty, bgs, scrollY: sy };
}"#;

/// Per cell: None = not under text; Some(bg) = under a text line, bg = its solid background if known.
type BoxBg = Option<Option<[u8; 3]>>;
type Rgb = [u8; 3];

struct Run {
    row: usize,
    col: usize,
    text: String,
    color: Option<Rgb>,
}

/// One terminal cell as drawn. ch == '\0': right half of a wide char (nothing is printed there).
#[derive(Clone, Copy, PartialEq, Debug)]
struct Out {
    ch: char,
    fg: Rgb,
    bg: Rgb,
}
const STALE: Out = Out { ch: '\u{1}', fg: [0; 3], bg: [0; 3] }; // "unknown on screen": always redrawn

fn macos_major() -> u32 {
    let plist = std::fs::read_to_string("/System/Library/CoreServices/SystemVersion.plist").unwrap_or_default();
    let after = plist.split("<key>ProductVersion</key>").nth(1).unwrap_or("");
    let v = after.split("<string>").nth(1).unwrap_or("");
    v.split('.').next().and_then(|m| m.parse().ok()).unwrap_or(0)
}

fn truecolor() -> bool {
    std::env::var("COLORTERM").is_ok_and(|c| c.contains("truecolor") || c.contains("24bit"))
        // Terminal.app does 24-bit color since macOS 26 but doesn't set COLORTERM
        || (std::env::var("TERM_PROGRAM").as_deref() == Ok("Apple_Terminal") && macos_major() >= 26)
}

/// "rgb(1, 2, 3)" / "rgba(1, 2, 3, 0.5)" -> color; fully transparent (e.g. gradient text) -> None.
fn parse_css_color(s: &str) -> Option<Rgb> {
    let inner = s.split_once('(')?.1.trim_end_matches(')');
    let v: Vec<f64> = inner.split(',').filter_map(|p| p.trim().parse().ok()).collect();
    if v.len() < 3 || v.get(3) == Some(&0.0) {
        return None;
    }
    Some([v[0] as u8, v[1] as u8, v[2] as u8])
}

fn sgr(fg: bool, c: Rgb, truecolor: bool) -> String {
    let k = if fg { 38 } else { 48 };
    if truecolor {
        format!("\x1b[{k};2;{};{};{}m", c[0], c[1], c[2])
    } else {
        format!("\x1b[{k};5;{}m", ansi_colours::ansi256_from_rgb((c[0], c[1], c[2])))
    }
}

/// Block Elements glyphs as 8x8 coverage masks (bit y*8+x set = foreground). They're in every common
/// monospace font (Menlo, SF Mono, Consolas, DejaVu), unlike sextants/octants. Complements are implied:
/// swapping fg/bg turns ▀ into ▄, so each pair is listed once.
const SYMBOLS: [(char, u64); 20] = {
    const fn rect(x0: u32, y0: u32, x1: u32, y1: u32) -> u64 {
        let mut m = 0u64;
        let mut y = y0;
        while y < y1 {
            let mut x = x0;
            while x < x1 {
                m |= 1 << (y * 8 + x);
                x += 1;
            }
            y += 1;
        }
        m
    }
    let (ul, ur, ll, lr) = (rect(0, 0, 4, 4), rect(4, 0, 8, 4), rect(0, 4, 4, 8), rect(4, 4, 8, 8));
    [
        (' ', 0),
        ('▀', ul | ur),
        ('▌', ul | ll),
        ('▘', ul),
        ('▝', ur),
        ('▖', ll),
        ('▗', lr),
        ('▚', ul | lr),
        ('▁', rect(0, 7, 8, 8)),
        ('▂', rect(0, 6, 8, 8)),
        ('▃', rect(0, 5, 8, 8)),
        ('▅', rect(0, 3, 8, 8)),
        ('▆', rect(0, 2, 8, 8)),
        ('▇', rect(0, 1, 8, 8)),
        ('▏', rect(0, 0, 1, 8)),
        ('▎', rect(0, 0, 2, 8)),
        ('▍', rect(0, 0, 3, 8)),
        ('▋', rect(0, 0, 5, 8)),
        ('▊', rect(0, 0, 6, 8)),
        ('▉', rect(0, 0, 7, 8)),
    ]
};

/// Best glyph + 2 colors for one image cell: chafa-style least squared error over SYMBOLS.
/// About 2x the horizontal detail of plain half blocks, and edges land on 1/8-cell steps.
fn fit_cell(img: &image::RgbImage, x0: usize, x1: usize, y0: usize, y1: usize) -> Out {
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    let mut px = [[0f32; 3]; 64];
    let (mut sum, mut sq) = ([0f32; 3], 0f32);
    for (i, p) in px.iter_mut().enumerate() {
        let (sx, sy) = (i % 8, i / 8); // 8x8 sample grid over the cell
        let x = (x0 + (2 * sx + 1) * (x1 - x0).max(1) / 16).min(iw - 1);
        let y = (y0 + (2 * sy + 1) * (y1 - y0).max(1) / 16).min(ih - 1);
        let c = img.get_pixel(x as u32, y as u32).0;
        *p = [c[0] as f32, c[1] as f32, c[2] as f32];
        for k in 0..3 {
            sum[k] += p[k];
            sq += p[k] * p[k];
        }
    }
    let mut best = (f32::MAX, ' ', [0f32; 3], [0f32; 3]);
    for &(ch, mask) in &SYMBOLS {
        let (mut f, mut nf) = ([0f32; 3], 0f32);
        for (i, p) in px.iter().enumerate() {
            if mask >> i & 1 == 1 {
                (0..3).for_each(|k| f[k] += p[k]);
                nf += 1.0;
            }
        }
        let nb = 64.0 - nf;
        let b = [sum[0] - f[0], sum[1] - f[1], sum[2] - f[2]];
        // squared error with each side painted its mean = sum(p^2) - |S_f|^2/n_f - |S_b|^2/n_b
        let part = |s: [f32; 3], n: f32| if n > 0.0 { (s[0] * s[0] + s[1] * s[1] + s[2] * s[2]) / n } else { 0.0 };
        let err = sq - part(f, nf) - part(b, nb);
        if err < best.0 - 1e-3 {
            let mean = |s: [f32; 3], n: f32| s.map(|v| v / n.max(1.0));
            best = (err, ch, mean(f, nf), mean(b, nb));
        }
    }
    let rgb = |c: [f32; 3]| c.map(|v| v.round().clamp(0.0, 255.0) as u8);
    let (fg, bg) = (rgb(best.2), rgb(best.3));
    if best.1 == ' ' { Out { ch: ' ', fg: bg, bg } } else { Out { ch: best.1, fg, bg } }
}

/// Screenshot + text runs -> cells. Text: real glyph on its line's background; other cells under a text
/// line: that flat background (no glyph smudge); everything else: best-fitting block glyph.
fn render_text_mode(img: &image::RgbImage, cols: usize, rows: usize, runs: &[Run], texty: &[BoxBg]) -> Vec<Out> {
    use unicode_width::UnicodeWidthChar;
    enum Cell {
        Img,
        Char(char, Option<Rgb>),
        Wide, // second half of a double-width char
    }
    let mut grid: Vec<Cell> = (0..cols * rows).map(|_| Cell::Img).collect();
    let used = |g: &[Cell], i: usize| !matches!(g[i], Cell::Img);
    let mut runs: Vec<&Run> = runs.iter().filter(|r| r.row < rows).collect();
    runs.sort_by_key(|r| (r.row, r.col)); // left to right, so the gap rule below sees what's really to the left
    for r in runs {
        let row = &grid[r.row * cols..(r.row + 1) * cols];
        // start after (plus a gap) whatever an earlier run on this row already filled
        let mut c = r.col;
        while c < cols && used(row, c) {
            c += 1;
        }
        if c > 0 && c < cols && used(row, c - 1) {
            c += 1;
        }
        for ch in r.text.chars() {
            let w = ch.width().unwrap_or(0);
            if w == 0 {
                continue; // combining marks / control chars would shift the row
            }
            while c + w <= cols && (c..c + w).any(|k| used(&grid[r.row * cols..], k)) {
                c += 1; // collided with an earlier run: shift right instead of dropping the char
            }
            if c + w > cols {
                break;
            }
            let i = r.row * cols + c;
            grid[i] = Cell::Char(ch, r.color);
            if w == 2 {
                grid[i + 1] = Cell::Wide;
            }
            c += w;
        }
    }
    let (iw, ih) = (img.width() as usize, img.height() as usize);
    // dominant color: most common 4-bit-per-channel bin, returned as that bin's mean
    let dominant = |x0: usize, x1: usize, y0: usize, y1: usize| {
        let mut bins: Vec<(u16, u32, [u64; 3])> = Vec::new();
        for y in y0..y1.max(y0 + 1).min(ih) {
            for x in x0..x1.max(x0 + 1).min(iw) {
                let p = img.get_pixel(x as u32, y as u32).0;
                let key = ((p[0] as u16 >> 4) << 8) | ((p[1] as u16 >> 4) << 4) | (p[2] as u16 >> 4);
                match bins.iter_mut().find(|b| b.0 == key) {
                    Some(b) => {
                        b.1 += 1;
                        (0..3).for_each(|k| b.2[k] += p[k] as u64);
                    }
                    None => bins.push((key, 1, [p[0] as u64, p[1] as u64, p[2] as u64])),
                }
            }
        }
        let b = bins.iter().max_by_key(|b| b.1).map_or((0, 1, [0; 3]), |b| *b);
        let n = b.1.max(1) as u64;
        [(b.2[0] / n) as u8, (b.2[1] / n) as u8, (b.2[2] / n) as u8]
    };
    let mut out = Vec::with_capacity(cols * rows);
    for row in 0..rows {
        for col in 0..cols {
            let (x0, x1) = (col * iw / cols, (col + 1) * iw / cols);
            let (y0, y1) = (row * ih / rows, (row + 1) * ih / rows);
            let boxed = texty.get(row * cols + col).copied().flatten();
            let flat = || boxed.flatten().unwrap_or_else(|| dominant(x0, x1, y0, y1));
            out.push(match grid[row * cols + col] {
                Cell::Wide => Out { ch: '\0', fg: [0; 3], bg: [0; 3] },
                Cell::Char(ch, color) => {
                    let bg = flat();
                    let lum = bg[0] as u32 * 3 + bg[1] as u32 * 6 + bg[2] as u32; // ~luma x10
                    let fg = color.unwrap_or(if lum > 1400 { [20, 20, 20] } else { [235, 235, 235] });
                    Out { ch, fg, bg }
                }
                Cell::Img if boxed.is_some() => {
                    let bg = flat();
                    Out { ch: ' ', fg: bg, bg }
                }
                Cell::Img => fit_cell(img, x0, x1, y0, y1),
            });
        }
    }
    out
}

/// Terminal bytes that turn `prev` (what's on screen) into `cur`. `shift` = rows the content moved up
/// (scrolling down; negative = down): the terminal moves those rows itself (scroll region + delete/insert
/// line, VT102), so a scroll step only repaints the rows that came into view plus what really changed.
fn emit(prev: &mut Vec<Out>, cur: &[Out], cols: usize, rows: usize, shift: i32, truecolor: bool) -> String {
    use unicode_width::UnicodeWidthChar;
    let mut out = String::new();
    if prev.len() != cur.len() {
        *prev = vec![STALE; cur.len()];
    } else if shift != 0 && (shift.unsigned_abs() as usize) < rows {
        let k = shift.unsigned_abs() as usize;
        out += &format!("\x1b[1;{rows}r\x1b[1;1H");
        if shift > 0 {
            out += &format!("\x1b[{k}M");
            prev.drain(..k * cols);
            prev.extend(std::iter::repeat(STALE).take(k * cols));
        } else {
            out += &format!("\x1b[{k}L");
            prev.truncate((rows - k) * cols);
            prev.splice(0..0, std::iter::repeat(STALE).take(k * cols));
        }
        out += "\x1b[r";
    } else if shift != 0 {
        prev.fill(STALE);
    }
    let (mut at, mut fg, mut bg): (Option<(usize, usize)>, Option<Rgb>, Option<Rgb>) = (None, None, None);
    for row in 0..rows {
        let line = row * cols..(row + 1) * cols;
        // a double-width glyph partly overwritten leaves junk: redraw such rows whole
        let wide = |cells: &[Out]| cells.iter().any(|o| o.ch == '\0');
        let whole = wide(&prev[line.clone()]) || wide(&cur[line.clone()]);
        for col in 0..cols {
            let (i, o) = (row * cols + col, cur[row * cols + col]);
            if o.ch == '\0' || (!whole && prev[i] == o) {
                continue;
            }
            if at != Some((row, col)) {
                out += &format!("\x1b[{};{}H", row + 1, col + 1);
            }
            if bg != Some(o.bg) {
                out += &sgr(false, o.bg, truecolor);
                bg = Some(o.bg);
            }
            if o.ch != ' ' && fg != Some(o.fg) {
                out += &sgr(true, o.fg, truecolor);
                fg = Some(o.fg);
            }
            out.push(o.ch);
            let next = col + o.ch.width().unwrap_or(1).max(1);
            at = (next < cols).then_some((row, next)); // after the last column the cursor position is unreliable
        }
    }
    if !out.is_empty() {
        out += "\x1b[0m";
    }
    prev.copy_from_slice(cur);
    out
}

fn main() {
    // VS Code's terminal draws iTerm inline images (terminal.integrated.enableImages) but viuer doesn't know it.
    // Set before viuer's first probe; we're still single-threaded here.
    if std::env::var("TERM_PROGRAM").as_deref() == Ok("vscode") && std::env::var_os("LC_TERMINAL").is_none() {
        std::env::set_var("LC_TERMINAL", "iTerm2");
    }
    let url = std::env::args().nth(1).map(|a| to_url(&a));
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .map(|a| a.port())
        .expect("no free local port");
    let bin = obscura_bin();
    let child = Command::new(&bin)
        .args(["serve", "--host", "127.0.0.1", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let _engine = match child {
        Ok(c) => Obscura(c),
        Err(e) => {
            eprintln!(
                "cannot start {bin:?}: {e}\nDownload the render build from https://github.com/h4ckf0r0day/obscura/releases,\n\
                 then put `obscura` on PATH, next to tbrowse, or set OBSCURA=/path/to/obscura"
            );
            std::process::exit(1);
        }
    };
    let cdp = connect(port).unwrap_or_else(|e| {
        eprintln!("cannot reach obscura CDP on port {port}: {e}");
        std::process::exit(1);
    });

    // Run viuer's terminal capability probes now, before raw mode + mouse capture own stdin.
    let _ = (viuer::get_kitty_support(), viuer::is_iterm_supported(), viuer::is_sixel_supported());
    let _screen = Screen::enter().expect("cannot set up terminal");

    let mut tab = Tab { cdp, v: view(), last: vec![], printed: (1, 1), cells: vec![], scroll_y: 0.0, truecolor: truecolor() };
    tab.resize();
    match url.or_else(|| prompt(&tab.v, " url or search: ").map(|u| to_url(&u))) {
        Some(u) => tab.go(&u),
        None => tab.draw(),
    }

    let mut next_draw = Instant::now() + Duration::from_secs(1);
    'main: loop {
        if !event::poll(Duration::from_millis(50)).unwrap_or(false) {
            // JS changes pages on its own (timers, fetch, clicks that navigate): keep repainting
            if Instant::now() >= next_draw {
                tab.draw();
                next_draw = Instant::now() + Duration::from_secs(1);
            }
            continue;
        }
        let soon = Instant::now() + Duration::from_millis(120);
        let page_rows = tab.v.page_rows as i32;
        let mut scroll = (0i32, tab.v.css_w as f64 / 2.0, tab.v.css_h as f64 / 2.0); // rows, at x, y
        // drain everything queued: a burst of wheel ticks becomes one scroll and one frame
        while event::poll(Duration::ZERO).unwrap_or(false) {
            match event::read() {
                Ok(Event::Resize(..)) => {
                    tab.resize();
                    tab.draw();
                }
                Ok(Event::Mouse(m)) => {
                    let (x, y) = to_css(&tab.v, tab.printed, m.column, m.row);
                    match m.kind {
                        MouseEventKind::Down(MouseButton::Left) if m.row < tab.v.page_rows => {
                            for t in ["mouseMoved", "mousePressed", "mouseReleased"] {
                                let _ = tab.cdp.call(
                                    "Input.dispatchMouseEvent",
                                    json!({ "type": t, "x": x, "y": y, "button": "left", "clickCount": 1 }),
                                );
                            }
                            next_draw = soon;
                        }
                        MouseEventKind::ScrollDown => scroll = (scroll.0 + 3, x, y),
                        MouseEventKind::ScrollUp => scroll = (scroll.0 - 3, x, y),
                        _ => {}
                    }
                }
                Ok(Event::Key(k)) if k.kind == KeyEventKind::Press => {
                    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                    let alt = k.modifiers.contains(KeyModifiers::ALT);
                    match k.code {
                        KeyCode::Char('q' | 'c') if ctrl => break 'main,
                        KeyCode::Char('l') if ctrl => match prompt(&tab.v, " url or search: ") {
                            Some(u) => tab.go(&to_url(&u)),
                            None => tab.draw(),
                        },
                        KeyCode::Char('r') if ctrl => {
                            let u = tab.cdp.eval("location.href");
                            tab.go(u.as_str().unwrap_or("about:blank"));
                        }
                        KeyCode::Char('b') if ctrl => {
                            tab.cdp.eval("history.back()");
                        }
                        KeyCode::Left if alt => {
                            tab.cdp.eval("history.back()");
                        }
                        KeyCode::PageDown => scroll.0 += page_rows - 2,
                        KeyCode::PageUp => scroll.0 -= page_rows - 2,
                        KeyCode::Down => scroll.0 += 1,
                        KeyCode::Up => scroll.0 -= 1,
                        KeyCode::Home => {
                            tab.cdp.eval("scrollTo(0, 0)");
                        }
                        KeyCode::End => {
                            tab.cdp.eval("scrollTo(0, document.documentElement.scrollHeight)");
                        }
                        KeyCode::Enter => {
                            tab.cdp.key("Enter", 13);
                            // Obscura 0.2.7 doesn't submit forms on Enter, so submit the focused field's form ourselves
                            tab.cdp.eval("(e => e && e.form && e.form.requestSubmit())(document.activeElement)");
                        }
                        KeyCode::Backspace => tab.cdp.key("Backspace", 8),
                        KeyCode::Char(c) if !ctrl && !alt => {
                            let _ = tab.cdp.call("Input.dispatchKeyEvent", json!({ "type": "char", "text": c.to_string() }));
                        }
                        _ => {}
                    }
                    next_draw = next_draw.min(soon);
                }
                _ => {}
            }
        }
        if scroll.0 != 0 {
            tab.scroll_rows(scroll.0, scroll.1, scroll.2);
            tab.draw(); // right away: scrolling must feel immediate
            next_draw = Instant::now() + Duration::from_secs(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_or_search() {
        assert_eq!(to_url("https://a.b/c"), "https://a.b/c");
        assert_eq!(to_url("about:blank"), "about:blank");
        assert_eq!(to_url(" news.ycombinator.com "), "https://news.ycombinator.com");
        assert_eq!(to_url("rust lang"), "https://html.duckduckgo.com/html/?q=rust%20lang");
        assert_eq!(to_url("phở"), "https://html.duckduckgo.com/html/?q=ph%E1%BB%9F");
    }

    #[test]
    fn cell_maps_to_page_pixel() {
        let v = View { cols: 100, page_rows: 30, css_w: 800, css_h: 480, dpr: 1, sharp: false };
        assert_eq!(to_css(&v, (100, 30), 0, 0), (4.0, 8.0));
        assert_eq!(to_css(&v, (100, 30), 99, 29), (796.0, 472.0));
        // viuer printed a narrower image (aspect kept): mapping follows the printed size
        assert_eq!(to_css(&v, (50, 30), 49, 0), (792.0, 8.0));
    }

    #[test]
    fn css_colors() {
        assert_eq!(parse_css_color("rgb(91, 107, 127)"), Some([91, 107, 127]));
        assert_eq!(parse_css_color("rgba(1, 2, 3, 0.5)"), Some([1, 2, 3]));
        assert_eq!(parse_css_color("rgba(0, 0, 0, 0)"), None);
        assert_eq!(parse_css_color("garbage"), None);
    }

    #[test]
    fn text_mode_cells() {
        // 3 cells x 1 row, 8x16 px each: white | white | red
        let img = image::RgbImage::from_fn(24, 16, |x, _| if x >= 16 { image::Rgb([255, 0, 0]) } else { image::Rgb([255, 255, 255]) });
        let line = |cells: &[Out]| cells.iter().filter(|o| o.ch != '\0').map(|o| o.ch).collect::<String>();
        let runs = [Run { row: 0, col: 0, text: "Hi there".into(), color: Some([0, 0, 255]) }];
        let out = render_text_mode(&img, 3, 1, &runs, &[Some(None), Some(None), None]);
        // as many chars as fit, glyph in the text color on the cell's background
        assert_eq!(line(&out), "Hi ");
        assert_eq!(out[0], Out { ch: 'H', fg: [0, 0, 255], bg: [255, 255, 255] });
        // flat image cell -> plain background
        assert_eq!(render_text_mode(&img, 3, 1, &[], &[None; 3])[2], Out { ch: ' ', fg: [255, 0, 0], bg: [255, 0, 0] });
        // runs keep a one-cell gap after an earlier run on the row
        let blank = image::RgbImage::from_pixel(64, 16, image::Rgb([255, 255, 255]));
        let run = |col, t: &str| Run { row: 0, col, text: t.into(), color: Some([0, 0, 0]) };
        assert_eq!(line(&render_text_mode(&blank, 8, 1, &[run(5, "abcdef")], &[Some(None); 8])), "     abc");
        assert_eq!(line(&render_text_mode(&blank, 8, 1, &[run(0, "abcd"), run(2, "xy")], &[Some(None); 8])), "abcd xy ");
        // wide char takes two cells, transparent color falls back to contrast
        let runs = [Run { row: 0, col: 0, text: "中a".into(), color: None }];
        let out = render_text_mode(&img, 3, 1, &runs, &[Some(None); 3]);
        assert_eq!(line(&out), "中a");
        assert_eq!(out[0].fg, [20, 20, 20]);
        // a known line background wins over the cell's pixels (big glyph strokes would otherwise win)
        assert_eq!(render_text_mode(&img, 3, 1, &[], &[None, None, Some(Some([1, 2, 3]))])[2].bg, [1, 2, 3]);
    }

    #[test]
    fn block_fitting() {
        let w = image::Rgb([255, 255, 255]);
        let k = image::Rgb([0, 0, 0]);
        let cell = |f: fn(u32, u32) -> bool| {
            let img = image::RgbImage::from_fn(8, 16, |x, y| if f(x, y) { k } else { w });
            fit_cell(&img, 0, 8, 0, 16)
        };
        // left half dark: the ▌ glyph, dark fg on white bg (plain half blocks can't do vertical splits)
        assert_eq!(cell(|x, _| x < 4), Out { ch: '▌', fg: [0, 0, 0], bg: [255, 255, 255] });
        // bottom-right quadrant: ▗
        assert_eq!(cell(|x, y| x >= 4 && y >= 8).ch, '▗');
        // thin bottom bar (1/8 of the height): ▁
        assert_eq!(cell(|_, y| y >= 14).ch, '▁');
        // uniform: a space in that color
        assert_eq!(cell(|_, _| false), Out { ch: ' ', fg: [255, 255, 255], bg: [255, 255, 255] });
    }

    #[test]
    fn diff_and_scroll() {
        let o = |ch| Out { ch, fg: [0; 3], bg: [9, 9, 9] };
        let frame = |s: &str| s.chars().map(o).collect::<Vec<_>>();
        let mut prev = vec![];
        let first = emit(&mut prev, &frame("abcdef"), 2, 3, 0, true); // 2 cols x 3 rows
        assert!(first.contains('a') && first.contains('f'));
        assert_eq!(emit(&mut prev, &frame("abcdef"), 2, 3, 0, true), "", "nothing changed -> no bytes");
        let one = emit(&mut prev, &frame("abcdXf"), 2, 3, 0, true);
        assert!(one.starts_with("\x1b[3;1H") && one.ends_with("X\x1b[0m") && !one.contains('a'), "only the changed cell: {one:?}");
        // scrolled down one row: the terminal deletes the top line, only the new bottom row is sent
        let s = emit(&mut prev, &frame("cdXfgh"), 2, 3, 1, true);
        assert!(s.starts_with("\x1b[1;3r\x1b[1;1H\x1b[1M\x1b[r"), "{s:?}");
        assert!(s.contains('g') && s.contains('h') && !s.contains('c') && !s.contains('X'), "{s:?}");
        // and back up: insert a line at the top
        let s = emit(&mut prev, &frame("abcdXf"), 2, 3, -1, true);
        assert!(s.contains("\x1b[1L") && s.contains('a') && s.contains('b') && !s.contains('X'), "{s:?}");
    }
}
