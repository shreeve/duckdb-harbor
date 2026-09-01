//! Wide-table probe: 500 columns x 100,000 rows through gpui-component's
//! Table (COMPONENTS.md mandates measuring before trusting it). The window
//! drives itself: six scroll patterns, frame deltas recorded via an
//! `on_next_frame` chain, report printed to stdout, then the app quits.
//!
//! Run: cargo run --release -p ducktable --example wide_probe

use std::time::Instant;

use gpui::*;
use gpui_component::table::{Column, Table, TableDelegate, TableState};
use gpui_component::Root;

const COLS: usize = 500;
const ROWS: usize = 100_000;

#[derive(Clone, Copy, Debug)]
enum Phase {
    Warmup,
    VSmooth,
    VJump,
    HSmooth,
    HJump,
    XYJump,
}

const PHASES: &[(Phase, usize)] = &[
    (Phase::Warmup, 60),
    (Phase::VSmooth, 240),
    (Phase::VJump, 240),
    (Phase::HSmooth, 240),
    (Phase::HJump, 240),
    (Phase::XYJump, 240),
];

struct Probe {
    cols: Vec<Column>,
}

impl Probe {
    fn new() -> Self {
        let cols = (0..COLS)
            .map(|i| Column::new(format!("c{i}"), format!("col_{i:03}")))
            .collect();
        Self { cols }
    }
}

impl TableDelegate for Probe {
    fn columns_count(&self, _: &App) -> usize {
        COLS
    }

    fn rows_count(&self, _: &App) -> usize {
        ROWS
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.cols[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        // Deterministic varied-width values generated on demand, the way a
        // paged result set would hand them to the grid.
        let h = row_ix.wrapping_mul(2654435761) ^ col_ix.wrapping_mul(40503);
        let text = match col_ix % 5 {
            0 => format!("{}", h % 1_000_000),
            1 => format!("item_{row_ix}_{col_ix}"),
            2 => format!("{}.{:02}", h % 10_000, h % 100),
            3 => (if h & 1 == 0 { "true" } else { "false" }).to_string(),
            _ => format!("cell r{row_ix} c{col_ix}"),
        };
        div().child(text)
    }
}

struct ProbeApp {
    table: Entity<TableState<Probe>>,
    started: bool,
    phase: usize,
    step: usize,
    last: Option<Instant>,
    frames: Vec<(usize, f32)>,
    seed: u64,
}

impl ProbeApp {
    fn rng(&mut self) -> usize {
        self.seed = self
            .seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.seed >> 33) as usize
    }

    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let now = Instant::now();
        if let Some(last) = self.last {
            self.frames
                .push((self.phase, (now - last).as_secs_f32() * 1000.0));
        }
        self.last = Some(now);

        self.step += 1;
        if self.step >= PHASES[self.phase].1 {
            self.step = 0;
            self.phase += 1;
            self.last = None;
            if self.phase >= PHASES.len() {
                self.report();
                cx.quit();
                return;
            }
        }

        let step = self.step;
        let (target_row, target_col) = match PHASES[self.phase].0 {
            Phase::Warmup => (None, None),
            Phase::VSmooth => (Some(step * 3 % ROWS), None),
            Phase::VJump => (Some(self.rng() % ROWS), None),
            Phase::HSmooth => (None, Some(step * 2 % COLS)),
            Phase::HJump => (None, Some(self.rng() % COLS)),
            Phase::XYJump => {
                let r = self.rng() % ROWS;
                let c = self.rng() % COLS;
                (Some(r), Some(c))
            }
        };
        self.table.update(cx, |table, cx| {
            if let Some(r) = target_row {
                table.scroll_to_row(r, cx);
            }
            if let Some(c) = target_col {
                table.scroll_to_col(c, cx);
            }
            cx.notify();
        });
        cx.notify();

        let this = cx.entity();
        window.on_next_frame(move |window, cx| {
            this.update(cx, |view, cx| view.tick(window, cx));
        });
    }

    fn report(&self) {
        println!();
        println!("wide_probe: {COLS} cols x {ROWS} rows, gpui-component Table");
        println!(
            "{:<10} {:>7} {:>8} {:>8} {:>8} {:>8} {:>7} {:>7}",
            "phase", "frames", "mean", "p50", "p95", "max", ">17ms", ">33ms"
        );
        for (i, (phase, _)) in PHASES.iter().enumerate() {
            let mut ms: Vec<f32> = self
                .frames
                .iter()
                .filter(|(p, _)| *p == i)
                .map(|(_, m)| *m)
                .collect();
            if ms.is_empty() {
                continue;
            }
            ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let n = ms.len();
            let mean: f32 = ms.iter().sum::<f32>() / n as f32;
            let p50 = ms[n / 2];
            let p95 = ms[((n as f32 * 0.95) as usize).min(n - 1)];
            let max = ms[n - 1];
            let over17 = ms.iter().filter(|m| **m > 17.0).count();
            let over33 = ms.iter().filter(|m| **m > 33.0).count();
            println!(
                "{:<10} {:>7} {:>7.1}m {:>7.1}m {:>7.1}m {:>7.1}m {:>6.1}% {:>6.1}%",
                format!("{:?}", phase),
                n,
                mean,
                p50,
                p95,
                max,
                over17 as f32 * 100.0 / n as f32,
                over33 as f32 * 100.0 / n as f32,
            );
        }
        let rss = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        println!("rss: {:.1} MB", rss as f64 / 1024.0);
    }
}

impl Render for ProbeApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.started {
            self.started = true;
            let this = cx.entity();
            window.on_next_frame(move |window, cx| {
                this.update(cx, |view, cx| view.tick(window, cx));
            });
        }
        div().size_full().child(Table::new(&self.table))
    }
}

fn main() {
    let app = Application::new();

    app.run(move |cx| {
        gpui_component::init(cx);

        cx.spawn(async move |cx| {
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                        point(px(80.), px(80.)),
                        size(px(1280.), px(800.)),
                    ))),
                    ..Default::default()
                },
                |window, cx| {
                    let table = cx.new(|cx| TableState::new(Probe::new(), window, cx));
                    let view = cx.new(|_| ProbeApp {
                        table,
                        started: false,
                        phase: 0,
                        step: 0,
                        last: None,
                        frames: Vec::new(),
                        seed: 0x5eed_5eed_5eed_5eed,
                    });
                    cx.new(|cx| Root::new(view, window, cx))
                },
            )?;

            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
