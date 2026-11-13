//! The dashboard and transaction list in the terminal.

use crate::report::{self as rp, Filter, Level, Tx};
use crate::util::{cut, money};
use chrono::NaiveDate;
use rusqlite::Connection;

pub struct Style {
    pub b: &'static str,
    pub dim: &'static str,
    pub red: &'static str,
    pub yel: &'static str,
    pub grn: &'static str,
    pub r: &'static str,
}

impl Style {
    pub fn new(color: bool) -> Self {
        if color {
            Style {
                b: "\x1b[1m",
                dim: "\x1b[2m",
                red: "\x1b[31m",
                yel: "\x1b[33m",
                grn: "\x1b[32m",
                r: "\x1b[0m",
            }
        } else {
            Style {
                b: "",
                dim: "",
                red: "",
                yel: "",
                grn: "",
                r: "",
            }
        }
    }
}

/// The terminal width from $COLUMNS, between 80 and 120 (100 when unknown).
pub fn width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse().ok())
        .unwrap_or(100)
        .clamp(80, 120)
}

fn bar(value: f64, top: f64, w: usize) -> String {
    let n = if top > 0.0 && value > 0.0 {
        ((w as f64 * value / top).round() as usize).min(w)
    } else {
        0
    };
    "█".repeat(n) + &" ".repeat(w - n)
}

fn title(st: &Style, text: &str, w: usize) -> String {
    format!(
        "{}── {text} {}{}{}{}",
        st.b,
        st.r,
        st.dim,
        "─".repeat(w.saturating_sub(text.chars().count() + 4)),
        st.r
    )
}

pub fn transaction_lines(rows: &[Tx], st: &Style, w: usize) -> Vec<String> {
    if rows.is_empty() {
        return vec![format!("{}(no transactions){}", st.dim, st.r)];
    }
    let dw = w.saturating_sub(10 + 1 + 20 + 1 + 18 + 1 + 13 + 1).max(16);
    let mut out = vec![format!(
        "{}{:<10} {:<20} {:<dw$} {:<18} {:>13}{}",
        st.dim, "DATE", "ACCOUNT", "DESCRIPTION", "CATEGORY", "AMOUNT", st.r
    )];
    for t in rows {
        let acct = t
            .account
            .as_ref()
            .map(|a| a.short())
            .unwrap_or_else(|| "?".into());
        let amount = -t.amount; // shown as you would read it: money in is +, money out is -
        let cat = if t.category.is_some() {
            rp::label(t.category.as_deref())
        } else {
            String::new()
        };
        out.push(format!(
            "{:<10} {:<20} {:<dw$} {:<18} {}{:>13}{}",
            t.date,
            cut(&acct, 20),
            cut(&t.description(), dw),
            cut(&cat, 18),
            if amount > 0.0 { st.grn } else { "" },
            money(Some(amount), true),
            st.r
        ));
    }
    out
}

pub fn dashboard(
    c: &Connection,
    today: NaiveDate,
    st: &Style,
    w: usize,
) -> rusqlite::Result<String> {
    let mut lines = vec![];
    let (last, banks) = rp::last_sync(c)?;
    lines.push(format!(
        "{}girfan{}  {}{today} · {banks} linked bank{}{}{}",
        st.b,
        st.r,
        st.dim,
        if banks == 1 { "" } else { "s" },
        last.map(|l| format!(
            " · synced {}",
            l.replace('T', " ").chars().take(16).collect::<String>()
        ))
        .unwrap_or_default(),
        st.r
    ));

    let alerts = rp::alerts(c, today)?;
    if !alerts.is_empty() {
        lines.push(title(st, "alerts", w));
        for (level, text) in alerts {
            lines.push(format!(
                "{}! {text}{}",
                if level == Level::Red { st.red } else { st.yel },
                st.r
            ));
        }
    }

    let (accounts, assets, debts) = rp::balances(c)?;
    lines.push(title(st, "balances", w));
    if accounts.is_empty() {
        lines.push(format!(
            "{}(no accounts yet: run girfan link, or girfan import FILE.ofx){}",
            st.dim, st.r
        ));
    }
    for a in &accounts {
        let extra = if rp::is_debt(a.kind.as_deref()) {
            format!(
                "owed{}",
                a.limit
                    .map(|l| format!(" of {} limit", money(Some(l), false)))
                    .unwrap_or_default()
            )
        } else {
            a.available
                .map(|v| format!("available {}", money(Some(v), false)))
                .unwrap_or_default()
        };
        lines.push(format!(
            "{:<22} {:<30} {:<12} {}{:>14}{}  {}{extra}{}",
            cut(a.institution.as_deref().unwrap_or(""), 22),
            cut(&a.short(), 30),
            cut(a.subtype.as_deref().or(a.kind.as_deref()).unwrap_or(""), 12),
            st.b,
            money(a.current, false),
            st.r,
            st.dim,
            st.r
        ));
    }
    if !accounts.is_empty() {
        lines.push(format!(
            "{d}you own{r} {} {d}· you owe{r} {} {d}· net worth{r} {b}{}{r}",
            money(Some(assets), false),
            money(Some(debts), false),
            money(Some(assets - debts), false),
            d = st.dim,
            r = st.r,
            b = st.b
        ));
    }

    let cards = rp::cards(c, today)?;
    let due_days = rp::setting(c, "due_days", rp::DUE_DAYS);
    if !cards.is_empty() {
        lines.push(title(st, "cards", w));
        lines.push(format!(
            "{}{:<34} {:>12} {:>5} {:>12} {:>10}  DUE{}",
            st.dim, "CARD", "BALANCE", "USED", "STATEMENT", "MINIMUM", st.r
        ));
        for k in &cards {
            let due = match k.days {
                Some(d) => format!(
                    "{} ({}){}",
                    k.due_date.as_deref().unwrap_or(""),
                    rp::when(d),
                    if k.paid { " paid" } else { "" }
                ),
                None => "-".into(),
            };
            let color = if k.overdue {
                st.red
            } else if k.due_soon(due_days) {
                st.yel
            } else {
                ""
            };
            lines.push(format!(
                "{:<34} {:>12} {:>5} {:>12} {:>10}  {color}{due}{}",
                cut(&k.account.title(), 34),
                money(k.account.current, false),
                k.used
                    .map(|u| format!("{:.0}%", u * 100.0))
                    .unwrap_or_else(|| "-".into()),
                money(k.statement_balance, false),
                money(k.minimum, false),
                st.r
            ));
        }
    }

    let months = rp::months(c, today, 6)?;
    if months.iter().any(|(_, v)| *v != 0.0) {
        lines.push(title(st, "spending", w));
        let top = months.iter().map(|(_, v)| *v).fold(0.0, f64::max);
        let bw = w.saturating_sub(30).clamp(10, 50);
        for (m, v) in &months {
            lines.push(format!(
                "{m}  {}{}{} {:>12}",
                st.grn,
                bar(*v, top, bw),
                st.r,
                money(Some(*v), false)
            ));
        }
        let cats = rp::categories(c, &months.last().expect("six months").0)?;
        if let Some(ctop) = cats.first().map(|(_, v)| *v) {
            lines.push(format!("{}this month by category:{}", st.dim, st.r));
            for (name, v) in cats.iter().take(8) {
                lines.push(format!(
                    "  {:<26} {} {:>12}",
                    cut(name, 26),
                    bar(*v, ctop, w.saturating_sub(46).clamp(8, 40)),
                    money(Some(*v), false)
                ));
            }
        }
    }

    lines.push(title(st, "recent transactions", w));
    lines.extend(transaction_lines(
        &rp::transactions(
            c,
            &Filter {
                limit: 12,
                ..Filter::default()
            },
        )?,
        st,
        w,
    ));
    Ok(lines.join("\n") + "\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tests::{fill, today};
    use crate::store::tests::TempDir;
    use crate::store::{Database, Env, Secrets};

    #[test]
    fn dashboard_shows_the_figures() {
        let t = TempDir::new();
        let ctx = t.ctx(Env::Production, "correct horse battery staple");
        let mut db = Database::open(&ctx, &mut Secrets::load(&ctx).unwrap()).unwrap();
        fill(&mut db);
        let text = dashboard(&db.conn, today(), &Style::new(false), 100).unwrap();
        for part in [
            "net worth",
            "$5,700.00",
            "Card Co Rewards ··3333",
            "this month by category",
            "-$600.00",
            "minimum $25.00",
        ] {
            assert!(text.contains(part), "missing {part}:\n{text}");
        }
    }
}
