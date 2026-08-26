//! A card per attached edge, redrawn once a second.
//!
//! Terminal escape codes and nothing else, for the same reason the rest of the
//! crate has no dependencies. The whole frame is built into one string and
//! written in one call, so a redraw does not tear.
//!
//! No alternate screen, no raw mode, and the cursor is left visible. The loop
//! runs until it is interrupted and never gets to undo anything, so it does
//! nothing that would need undoing: hiding the cursor here would leave a
//! terminal without one after Ctrl-C. The last frame stays on screen when the
//! process exits.

#![allow(dead_code)]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use umwelt::net::{EdgeView, RegionId};

/// Card width, not counting the space between columns.
const CARD: usize = 36;

/// Everything one redraw shows about the region itself.
pub struct Frame<'a> {
    pub region: RegionId,
    pub addr: String,
    pub tick_hz: u32,
    pub wait: &'a str,
    pub uptime: Duration,
    pub entities: usize,
    pub slots: usize,
    pub viewers: u64,
    pub records: u64,
    pub mean_ms: f64,
    pub worst_ms: f64,
    pub delivered: u64,
    pub dropped: u64,
    pub undeliverable: u64,
    pub refused: u64,
    pub edges: Vec<EdgeView>,
}

/// Holds the previous sample of each edge, so a card can show rates rather than
/// running totals. An edge that has gone is dropped from the map, so a reused
/// id does not inherit the previous occupant's numbers.
pub struct Dashboard {
    previous: HashMap<u32, (Instant, EdgeView)>,
    width: usize,
}

impl Dashboard {
    pub fn new(width: usize) -> Dashboard {
        Dashboard { previous: HashMap::new(), width: width.max(CARD) }
    }

    /// Clears the screen once, before the first frame.
    pub fn enter() -> &'static str {
        "\x1b[2J"
    }

    pub fn render(&mut self, f: &Frame<'_>) -> String {
        let now = Instant::now();
        let mut out = String::with_capacity(4096);
        // Home, then clear each line as it is written, so the frame replaces
        // the previous one without a blanking flash.
        out.push_str("\x1b[H");

        self.header(&mut out, f);

        let columns = ((self.width + 1) / (CARD + 1)).max(1);
        let cards: Vec<Vec<String>> = f
            .edges
            .iter()
            .map(|edge| self.card(edge, now))
            .collect();

        if cards.is_empty() {
            line(&mut out, "  waiting for edges");
        }
        for row in cards.chunks(columns) {
            let height = row.iter().map(|c| c.len()).max().unwrap_or(0);
            for r in 0..height {
                let mut joined = String::new();
                for card in row {
                    joined.push_str(card.get(r).map(String::as_str).unwrap_or(""));
                    joined.push(' ');
                }
                line(&mut out, joined.trim_end());
            }
        }

        // Forget edges that have gone, so a recycled id starts clean.
        let live: Vec<u32> = f.edges.iter().map(|e| e.id.raw()).collect();
        self.previous.retain(|id, _| live.contains(id));
        for edge in &f.edges {
            self.previous.insert(edge.id.raw(), (now, *edge));
        }

        // Clear anything the previous frame left below this one.
        out.push_str("\x1b[J");
        out
    }

    fn header(&self, out: &mut String, f: &Frame<'_>) {
        line(out, &format!("\x1b[1mumwelt {}\x1b[0m  {}", f.region, f.addr));
        line(
            out,
            &format!(
                "  {} Hz, wait {}, up {}",
                f.tick_hz,
                f.wait,
                span(f.uptime)
            ),
        );
        line(out, "");
        line(
            out,
            &format!(
                "  tick     mean {:.2} ms   worst {:.2} ms   of {} ms",
                f.mean_ms,
                f.worst_ms,
                1000 / f.tick_hz.max(1)
            ),
        );
        line(
            out,
            &format!(
                "  world    {} entities   {} slots   {} viewers/tick",
                thousands(f.entities as u64),
                thousands(f.slots as u64),
                thousands(f.viewers)
            ),
        );
        line(
            out,
            &format!(
                "  output   {} records/s   {} delivered   {} dropped",
                thousands(f.records),
                thousands(f.delivered),
                thousands(f.dropped)
            ),
        );
        line(
            out,
            &format!(
                "  declined {} refused   {} undeliverable",
                thousands(f.refused),
                thousands(f.undeliverable)
            ),
        );
        line(out, "");
        line(out, &format!("  \x1b[2m{} edge(s) attached\x1b[0m", f.edges.len()));
        line(out, "");
    }

    fn card(&self, edge: &EdgeView, now: Instant) -> Vec<String> {
        let rate = |current: u64, before: u64, secs: f64| -> f64 {
            if secs <= 0.0 {
                return 0.0;
            }
            current.saturating_sub(before) as f64 / secs
        };
        let (msg_s, frame_s, byte_s) = match self.previous.get(&edge.id.raw()) {
            Some((then, was)) => {
                let secs = now.duration_since(*then).as_secs_f64();
                (
                    rate(edge.messages, was.messages, secs),
                    rate(edge.frames, was.frames, secs),
                    rate(edge.bytes, was.bytes, secs),
                )
            }
            None => (0.0, 0.0, 0.0),
        };

        let title = format!("{:?} {}", edge.id, edge.peer);
        let mut card = vec![top(&title)];
        card.push(field("up", &span(edge.uptime)));
        card.push(field("entities", &thousands(edge.entities as u64)));
        card.push(field("observers", &thousands(edge.observers as u64)));
        card.push(field("in", &format!("{}/s", thousands(msg_s as u64))));
        card.push(field("out", &format!("{}/s", thousands(frame_s as u64))));
        card.push(field("", &format!("{}/s", bytes(byte_s))));
        let refused = if edge.refused == 0 {
            thousands(0)
        } else {
            format!("\x1b[33m{}\x1b[0m", thousands(edge.refused))
        };
        card.push(field_raw("refused", &refused, thousands(edge.refused).len()));
        card.push(bottom());
        card
    }
}

fn line(out: &mut String, text: &str) {
    out.push_str(text);
    out.push_str("\x1b[K\r\n");
}

/// `CARD` counts both border characters, so every line below is built to that
/// exact width. Getting one of the three wrong shows up immediately as a card
/// whose sides do not line up.
const INNER: usize = CARD - 2;

fn top(title: &str) -> String {
    // Inner content is "─", " ", title, " ", then the fill.
    let room = INNER - 3;
    let shown: String = title.chars().take(room).collect();
    let fill = room - shown.chars().count();
    format!("┌─ \x1b[1m{shown}\x1b[0m {}┐", "─".repeat(fill))
}

fn bottom() -> String {
    format!("└{}┘", "─".repeat(INNER))
}

fn field(label: &str, value: &str) -> String {
    field_raw(label, value, value.chars().count())
}

/// `width` is the printed width of `value`, which differs from its length when
/// it carries color codes.
fn field_raw(label: &str, value: &str, width: usize) -> String {
    // Inner content is " ", label, gap, value, " ".
    let gap = INNER.saturating_sub(2 + label.chars().count() + width);
    format!("│ {label}{}{value} │", " ".repeat(gap))
}

/// Digits in groups of three, so a six-figure rate is readable at a glance.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn bytes(per_sec: f64) -> String {
    const K: f64 = 1024.0;
    if per_sec >= K * K {
        format!("{:.1} MB", per_sec / (K * K))
    } else if per_sec >= K {
        format!("{:.1} KB", per_sec / K)
    } else {
        format!("{per_sec:.0} B")
    }
}

fn span(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{s}s")
    }
}

