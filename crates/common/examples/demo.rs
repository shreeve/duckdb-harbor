//! `cargo run -p harbor-common --example demo` — the look, without the plumbing.
fn main() {
    use harbor_common::state::State;
    use harbor_common::ui::*;
    let st = Style { color: std::env::args().any(|a| a == "--color"), boxed: true };

    let mut t = Table::new(["NAME", "STATE", "PID", "UPTIME", "DATABASE"]);
    // name, state, pid, uptime, database, note
    type Row<'a> = (&'a str, State, &'a str, &'a str, &'a str, Option<&'a str>);
    let rows: &[Row] = &[
        ("labs", State::Running, "83582", "4m", "~/Data/Code/invoices/data/labs.duckdb", None),
        ("medlabs", State::Drifted, "12699", "1h12m", "~/Data/Code/medlabs/api/db/medlabs.duckdb",
         Some("config now says medlabs-v2.duckdb — harbor stop medlabs && harbor start medlabs")),
        ("sales", State::Stopped, "—", "—", "~/Data/sales.duckdb", None),
        ("scratch", State::Unmanaged, "90551", "2m", "~/Data/scratch.duckdb",
         Some("not in config — summoned by pilot; retires 90s after the last use")),
        ("probe2", State::Stale, "—", "—", "—",
         Some("held but no longer configured — harbor forget probe2")),
    ];
    for (name, state, pid, up, db, note) in rows {
        t.row([
            Cell::new(name),
            Cell::new(state.label()).tone(Tone::from(state.level())),
            Cell::new(pid).right(),
            Cell::new(up).right(),
            Cell::new(db),
        ]);
        if let Some(n) = note {
            t.note(Tone::Dim, n);
        }
    }
    print!("{}", t.render(&st));
    println!("\n  2 running, 1 stopped, 1 unmanaged, 1 stale\n");

    let p = Panel::new("medlabs")
        .badge(format!("{} · 1h12m", State::Running.label()), Tone::from(State::Running.level()))
        .footer("pid 12699")
        .field("database", "~/Data/Code/medlabs/api/db/medlabs.duckdb  (412 MB)")
        .field("address", "unix ~/.local/state/harbor/runtime/medlabs.sock")
        .field("engine", "duckdb v2.0.0-alpha38195 · harbor 0.15.0")
        .field("limits", "6 workers · 2 GB · no statement timeout")
        .field("idle-exit", "never  (persistent)")
        .field("config", "~/.config/harbor/config.toml  [connection.medlabs]");
    print!("{}", p.render(&st));
}
