//! Output: one human view and one JSON view of the same rows.

use std::io::IsTerminal;

use crate::json::Json;
use crate::model::{Row, State, Window};
use crate::store::Store;
use crate::timefmt;

const BAR_WIDTH: usize = 14;

struct Style {
    on: bool,
}

impl Style {
    fn detect() -> Style {
        // NO_COLOR is honoured for any value, per no-color.org.
        Style {
            on: std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none(),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    /// Green below half, yellow approaching the limit, red at it. The colour is
    /// the fastest read on the line, so it tracks the decision — "can I keep
    /// working here" — not the raw number.
    fn by_load(&self, pct: f64, text: &str) -> String {
        let code = if pct >= 90.0 {
            "31"
        } else if pct >= 70.0 {
            "33"
        } else {
            "32"
        };
        self.paint(code, text)
    }
}

pub fn human_list(store_ref: &Store, rows: &[Row]) {
    let style = Style::detect();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            println!();
        }
        print_row(&style, row);
    }

    if store_ref.active().is_none() {
        println!();
        println!(
            "{}",
            style.dim("No account is marked active — run `cswap switch <account>`.")
        );
    }
}

fn print_row(style: &Style, row: &Row) {
    let marker = if row.active {
        style.bold("●")
    } else {
        style.dim("○")
    };
    let name = if row.active {
        style.bold(row.account.label())
    } else {
        row.account.label().to_string()
    };
    let mut header = format!("{marker} {}  {name}", row.account.num);
    if row.account.alias.is_some() {
        header.push_str(&style.dim(&format!("  {}", row.account.email)));
    }
    if row.active {
        header.push_str(&style.dim("  (active)"));
    }
    println!("{header}");

    match (&row.usage, &row.state) {
        (Some(usage), _) if !usage.is_empty() => {
            for window in usage.windows() {
                println!("    {}", window_line(style, window));
            }
            if let (State::Cached, Some(age)) = (&row.state, row.age()) {
                println!(
                    "    {}",
                    style.dim(&format!("measured {}", timefmt::ago(age)))
                );
            }
        }
        (_, state) => println!("    {}", style.dim(&note(state))),
    }
}

fn window_line(style: &Style, window: &Window) -> String {
    let bar = style.by_load(window.pct, &bar(window.pct));
    let pct = style.by_load(window.pct, &format!("{:>3.0}%", window.pct));
    let reset = match window.resets_at {
        Some(at) => {
            let remaining = at - timefmt::now_unix();
            style.dim(&format!(
                "resets in {}  ({})",
                timefmt::countdown(remaining),
                timefmt::utc_short(at)
            ))
        }
        None => String::new(),
    };
    format!("{:<6} {bar} {pct}  {reset}", window.label)
}

fn bar(pct: f64) -> String {
    let filled = ((pct / 100.0) * BAR_WIDTH as f64)
        .round()
        .clamp(0.0, BAR_WIDTH as f64) as usize;
    // A non-zero reading always shows at least one cell, so "barely used" and
    // "not used" stay distinguishable.
    let filled = if filled == 0 && pct > 0.0 { 1 } else { filled };
    format!("{}{}", "█".repeat(filled), "░".repeat(BAR_WIDTH - filled))
}

fn note(state: &State) -> String {
    match state {
        State::NoBackup => {
            "no stored login for this slot — log in as it and run `cswap add --slot N`".into()
        }
        State::ApiKey => "API key account — no subscription quota to report".into(),
        State::Dead => {
            "stored login has expired — log in as it again and re-run `cswap add`".into()
        }
        State::Error(message) => format!("usage unavailable ({message})"),
        State::Live | State::Cached => "no usage data".into(),
    }
}

pub fn json_list(store_ref: &Store, rows: &[Row]) -> Json {
    let mut out = Json::obj();
    out.set("schemaVersion", Json::num(1));
    out.set(
        "activeAccountNumber",
        store_ref.active().map_or(Json::Null, Json::num),
    );
    out.set("accounts", Json::Arr(rows.iter().map(json_row).collect()));
    out
}

fn json_row(row: &Row) -> Json {
    let mut entry = Json::obj();
    entry.set("number", Json::num(row.account.num));
    entry.set("email", Json::str(&row.account.email));
    if let Some(alias) = &row.account.alias {
        entry.set("alias", Json::str(alias));
    }
    if let Some(org) = &row.account.org_name {
        entry.set("organizationName", Json::str(org));
    }
    entry.set("active", Json::Bool(row.active));
    entry.set("usageStatus", Json::str(row.state.token()));

    match &row.usage {
        Some(usage) if !usage.is_empty() => {
            let mut block = Json::obj();
            if let Some(w) = &usage.five_hour {
                block.set("fiveHour", json_window(w));
            }
            if let Some(w) = &usage.seven_day {
                block.set("sevenDay", json_window(w));
            }
            if !usage.scoped.is_empty() {
                block.set(
                    "scoped",
                    Json::Arr(usage.scoped.iter().map(json_window).collect()),
                );
            }
            entry.set("usage", block);
            if let Some(at) = row.fetched_at {
                entry.set("usageFetchedAt", Json::str(timefmt::utc_stamp(at)));
                entry.set("usageAgeSeconds", Json::num(row.age().unwrap_or(0)));
            }
        }
        _ => entry.set("usage", Json::Null),
    }
    if let State::Error(message) = &row.state {
        entry.set("usageError", Json::str(message));
    }
    entry
}

fn json_window(window: &Window) -> Json {
    let mut out = Json::obj();
    out.set("label", Json::str(&window.label));
    out.set("pct", Json::Num(format!("{:.1}", window.pct)));
    match window.resets_at {
        Some(at) => {
            out.set("resetsAt", Json::str(timefmt::utc_stamp(at)));
            out.set(
                "resetsInSeconds",
                Json::num((at - timefmt::now_unix()).max(0)),
            );
        }
        None => out.set("resetsAt", Json::Null),
    }
    out
}

pub fn json_switch(switched: bool, from: Option<i64>, to: i64, reason: &str) -> Json {
    let mut out = Json::obj();
    out.set("schemaVersion", Json::num(1));
    out.set("switched", Json::Bool(switched));
    out.set("from", from.map_or(Json::Null, Json::num));
    out.set("to", Json::num(to));
    out.set("reason", Json::str(reason));
    out
}
