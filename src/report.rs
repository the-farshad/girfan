//! What the dashboards show, worked out from the database.
//!
//! Amounts follow Plaid: positive is money going out, negative is money coming in. Spending is
//! everything going out except transfers between your own accounts, income, and credit card
//! payments (the card purchases themselves are already counted).

use crate::util::money;
use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{Connection, Row};

pub const CARD_PAYMENT: &str = "LOAN_PAYMENTS_CREDIT_CARD_PAYMENT";
const NOT_SPENDING: &[&str] = &["INCOME", "TRANSFER_IN", "TRANSFER_OUT"];
pub const BIG_CHARGE: f64 = 500.0;
pub const DUE_DAYS: f64 = 5.0;

type R<T> = rusqlite::Result<T>;

pub fn is_spending(category: Option<&str>, detail: Option<&str>) -> bool {
    !category.is_some_and(|c| NOT_SPENDING.contains(&c)) && detail != Some(CARD_PAYMENT)
}

pub fn is_debt(kind: Option<&str>) -> bool {
    matches!(kind, Some("credit" | "loan"))
}

/// FOOD_AND_DRINK -> "Food and drink".
pub fn label(category: Option<&str>) -> String {
    let s = category
        .unwrap_or("UNCATEGORIZED")
        .replace('_', " ")
        .to_lowercase();
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

pub fn when(days: i64) -> String {
    match days {
        0 => "today".into(),
        1 => "tomorrow".into(),
        -1 => "yesterday".into(),
        d if d > 0 => format!("in {d} days"),
        d => format!("{} days ago", -d),
    }
}

pub struct Account {
    pub institution: Option<String>,
    pub name: Option<String>,
    pub mask: Option<String>,
    pub kind: Option<String>,
    pub subtype: Option<String>,
    pub current: Option<f64>,
    pub available: Option<f64>,
    pub limit: Option<f64>,
}

impl Account {
    fn from(r: &Row) -> R<Self> {
        Ok(Account {
            institution: r.get("institution")?,
            name: r.get("name")?,
            mask: r.get("mask")?,
            kind: r.get("type")?,
            subtype: r.get("subtype")?,
            current: r.get("current")?,
            available: r.get("available")?,
            limit: r.get("credit_limit")?,
        })
    }

    /// "Bank Card ··1234", without repeating a name that is also the bank's.
    pub fn title(&self) -> String {
        let (i, n) = (
            self.institution.as_deref().unwrap_or(""),
            self.name.as_deref().unwrap_or(""),
        );
        let base = if i == n || i.is_empty() {
            n.to_string()
        } else if n.is_empty() {
            i.to_string()
        } else {
            format!("{i} {n}")
        };
        match &self.mask {
            Some(m) => format!("{base} ··{m}"),
            None => base,
        }
    }

    pub fn short(&self) -> String {
        let n = self.name.as_deref().unwrap_or("");
        match &self.mask {
            Some(m) => format!("{n} ··{m}"),
            None => n.to_string(),
        }
    }
}

pub fn balances(c: &Connection) -> R<(Vec<Account>, f64, f64)> {
    let rows: Vec<Account> = c
        .prepare("SELECT * FROM accounts ORDER BY institution, type, name")?
        .query_map([], Account::from)?
        .collect::<R<_>>()?;
    let assets = rows
        .iter()
        .filter(|a| !is_debt(a.kind.as_deref()))
        .filter_map(|a| a.current)
        .sum();
    let debts = rows
        .iter()
        .filter(|a| is_debt(a.kind.as_deref()))
        .filter_map(|a| a.current)
        .sum();
    Ok((rows, assets, debts))
}

pub struct Card {
    pub account: Account,
    pub statement_balance: Option<f64>,
    pub minimum: Option<f64>,
    pub due_date: Option<String>,
    pub overdue: bool,
    pub days: Option<i64>,
    pub paid: bool,
    pub used: Option<f64>,
}

pub fn cards(c: &Connection, today: NaiveDate) -> R<Vec<Card>> {
    let mut st = c.prepare(
        "SELECT a.*, k.statement_balance, k.statement_date, k.minimum_payment, k.due_date, k.last_payment, \
         k.last_payment_date, k.overdue FROM accounts a LEFT JOIN cards k USING (account_id) \
         WHERE a.type = 'credit' ORDER BY a.institution, a.name")?;
    let rows = st.query_map([], |r| {
        let account = Account::from(r)?;
        let due: Option<String> = r.get("due_date")?;
        let (statement_date, last_date): (Option<String>, Option<String>) =
            (r.get("statement_date")?, r.get("last_payment_date")?);
        let (minimum, last): (Option<f64>, Option<f64>) =
            (r.get("minimum_payment")?, r.get("last_payment")?);
        let paid = matches!((&last_date, &statement_date), (Some(l), Some(s)) if l >= s)
            && last.unwrap_or(0.0) >= minimum.unwrap_or(0.0);
        let days = due
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .map(|d| (d - today).num_days());
        let used = match (account.current, account.limit) {
            (Some(cur), Some(lim)) if lim > 0.0 => Some(cur / lim),
            _ => None,
        };
        Ok(Card {
            statement_balance: r.get("statement_balance")?,
            minimum,
            due_date: due,
            overdue: r.get::<_, Option<bool>>("overdue")?.unwrap_or(false),
            days,
            paid,
            used,
            account,
        })
    })?;
    rows.collect()
}

pub fn setting(c: &Connection, key: &str, default: f64) -> f64 {
    c.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|v| v.parse().ok())
    .unwrap_or(default)
}

impl Card {
    pub fn due_soon(&self, due_days: f64) -> bool {
        self.days.is_some_and(|d| d >= 0 && (d as f64) <= due_days)
            && !self.paid
            && self.minimum.unwrap_or(0.0) > 0.0
    }
}

#[derive(PartialEq, Debug)]
pub enum Level {
    Red,
    Yellow,
}

/// What needs your attention: Red needs action now, Yellow is worth a look.
pub fn alerts(c: &Connection, today: NaiveDate) -> R<Vec<(Level, String)>> {
    let (big, due_days) = (
        setting(c, "big_charge", BIG_CHARGE),
        setting(c, "due_days", DUE_DAYS),
    );
    let mut out = vec![];
    for k in cards(c, today)? {
        if k.overdue {
            out.push((
                Level::Red,
                format!("{}: payment overdue", k.account.title()),
            ));
        } else if k.due_soon(due_days) {
            out.push((
                Level::Yellow,
                format!(
                    "{}: minimum {} due {} ({})",
                    k.account.title(),
                    money(k.minimum, false),
                    when(k.days.unwrap_or(0)),
                    k.due_date.as_deref().unwrap_or("")
                ),
            ));
        }
        if k.used.is_some_and(|u| u >= 0.9) {
            out.push((
                Level::Yellow,
                format!(
                    "{}: {:.0}% of the limit used",
                    k.account.title(),
                    k.used.unwrap_or(0.0) * 100.0
                ),
            ));
        }
    }
    let since = (today - Duration::days(7)).to_string();
    for t in transactions(
        c,
        &Filter {
            since: Some(since),
            min_amount: Some(big),
            limit: 100,
            ..Filter::default()
        },
    )? {
        if is_spending(t.category.as_deref(), t.detail.as_deref()) {
            let place = t
                .account
                .as_ref()
                .map(|a| format!(" ({})", a.title()))
                .unwrap_or_default();
            out.push((
                Level::Yellow,
                format!(
                    "large charge: {} at {} on {}{place}",
                    money(Some(t.amount), false),
                    t.description(),
                    t.date
                ),
            ));
        }
    }
    let mut st =
        c.prepare("SELECT item_id, institution, error, synced FROM items ORDER BY institution")?;
    let items = st.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;
    for item in items {
        let (id, inst, error, synced) = item?;
        let inst = inst.unwrap_or_default();
        if let Some(e) = error {
            let hint = if e == "ITEM_LOGIN_REQUIRED" {
                format!(
                    ": log in again with girfan link --update {}",
                    &id[..8.min(id.len())]
                )
            } else {
                String::new()
            };
            out.push((Level::Red, format!("{inst}: {e}{hint}")));
        } else if let Some(day) = synced
            .as_deref()
            .and_then(|s| s.get(..10))
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            && (today - day).num_days() > 3
        {
            out.push((
                Level::Yellow,
                format!("{inst}: last synced {day}; run girfan sync"),
            ));
        }
    }
    Ok(out)
}

/// "2026-06" … "2026-11": the last N months, this month last.
pub fn month_keys(today: NaiveDate, n: usize) -> Vec<String> {
    let (mut y, mut m) = (today.year(), today.month());
    let mut keys = vec![];
    for _ in 0..n {
        keys.push(format!("{y:04}-{m:02}"));
        (y, m) = if m > 1 { (y, m - 1) } else { (y - 1, 12) };
    }
    keys.reverse();
    keys
}

fn spending_by(c: &Connection, sql: &str, arg: &str) -> R<Vec<(String, f64)>> {
    let mut st = c.prepare(sql)?;
    let rows = st.query_map([arg], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })?;
    let mut out: Vec<(String, f64)> = vec![];
    for row in rows {
        let (key, category, detail, sum) = row?;
        if is_spending(category.as_deref(), detail.as_deref()) {
            match out.iter_mut().find(|(k, _)| *k == key) {
                Some((_, v)) => *v += sum,
                None => out.push((key, sum)),
            }
        }
    }
    Ok(out)
}

/// Spending per month for the last N months (this month included).
pub fn months(c: &Connection, today: NaiveDate, n: usize) -> R<Vec<(String, f64)>> {
    let keys = month_keys(today, n);
    let found = spending_by(
        c,
        "SELECT substr(date, 1, 7), category, detail, SUM(amount) FROM transactions \
                               WHERE date >= ?1 GROUP BY 1, 2, 3",
        &format!("{}-01", keys[0]),
    )?;
    Ok(keys
        .into_iter()
        .map(|k| {
            let v = found.iter().find(|(m, _)| *m == k).map_or(0.0, |(_, v)| *v);
            (k, v)
        })
        .collect())
}

/// Spending by category in MONTH ("2026-11"), largest first.
pub fn categories(c: &Connection, month: &str) -> R<Vec<(String, f64)>> {
    let mut out: Vec<(String, f64)> = vec![];
    for (cat, v) in spending_by(
        c,
        "SELECT COALESCE(category, 'UNCATEGORIZED'), category, detail, SUM(amount) FROM transactions \
                                    WHERE substr(date, 1, 7) = ?1 GROUP BY 1, 2, 3",
        month,
    )? {
        let name = label(Some(&cat));
        match out.iter_mut().find(|(k, _)| *k == name) {
            Some((_, x)) => *x += v,
            None => out.push((name, v)),
        }
    }
    out.retain(|(_, v)| v.abs() >= 0.005);
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(out)
}

pub struct Tx {
    pub date: String,
    pub name: String,
    pub merchant: Option<String>,
    pub amount: f64,
    pub category: Option<String>,
    pub detail: Option<String>,
    pub pending: bool,
    pub account: Option<Account>,
}

impl Tx {
    pub fn description(&self) -> String {
        self.merchant.clone().unwrap_or_else(|| self.name.clone())
            + if self.pending { " (pending)" } else { "" }
    }
}

#[derive(Default)]
pub struct Filter {
    pub text: Option<String>,
    pub since: Option<String>,
    pub account: Option<String>,
    pub min_amount: Option<f64>,
    pub limit: usize,
}

/// The latest transactions matching F: words in the description, merchant or category, a
/// first date, an account (bank, name or last 4 digits), and a smallest amount.
pub fn transactions(c: &Connection, f: &Filter) -> R<Vec<Tx>> {
    let mut sql = String::from(
        "SELECT t.date, t.name AS tname, t.merchant, t.amount, t.category, t.detail, t.pending, a.institution, a.name, \
         a.mask, a.type, a.subtype, a.current, a.available, a.credit_limit, t.account_id IN (SELECT account_id FROM accounts) AS known \
         FROM transactions t LEFT JOIN accounts a USING (account_id) WHERE 1 = 1",
    );
    let mut args: Vec<Box<dyn rusqlite::ToSql>> = vec![];
    if let Some(text) = f.text.as_deref().filter(|t| !t.trim().is_empty()) {
        let like = format!(
            "%{}%",
            text.split_whitespace().collect::<Vec<_>>().join("%")
        );
        sql += " AND (t.name LIKE ? OR t.merchant LIKE ? OR replace(t.category, '_', ' ') LIKE ?)";
        args.extend([
            Box::new(like.clone()) as Box<dyn rusqlite::ToSql>,
            Box::new(like.clone()),
            Box::new(like),
        ]);
    }
    if let Some(since) = &f.since {
        sql += " AND t.date >= ?";
        args.push(Box::new(since.clone()));
    }
    if let Some(acct) = f.account.as_deref().filter(|a| !a.trim().is_empty()) {
        sql += " AND (a.name LIKE ? OR a.institution LIKE ? OR a.mask = ?)";
        args.extend([
            Box::new(format!("%{acct}%")) as Box<dyn rusqlite::ToSql>,
            Box::new(format!("%{acct}%")),
            Box::new(acct.to_string()),
        ]);
    }
    if let Some(min) = f.min_amount {
        sql += " AND t.amount >= ?";
        args.push(Box::new(min));
    }
    sql += " ORDER BY t.date DESC, t.rowid DESC LIMIT ?";
    args.push(Box::new(f.limit as i64));
    let mut st = c.prepare(&sql)?;
    let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
        let known: bool = r.get("known")?;
        Ok(Tx {
            date: r.get("date")?,
            name: r.get::<_, Option<String>>("tname")?.unwrap_or_default(),
            merchant: r.get("merchant")?,
            amount: r.get("amount")?,
            category: r.get("category")?,
            detail: r.get("detail")?,
            pending: r.get::<_, Option<bool>>("pending")?.unwrap_or(false),
            account: if known { Some(Account::from(r)?) } else { None },
        })
    })?;
    rows.collect()
}

/// (latest sync time, number of linked banks).
pub fn last_sync(c: &Connection) -> R<(Option<String>, i64)> {
    c.query_row("SELECT MAX(synced), COUNT(*) FROM items", [], |r| {
        Ok((r.get(0)?, r.get(1)?))
    })
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::store::tests::TempDir;
    use crate::store::{Database, Env, Secrets};

    pub fn today() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 11, 13).unwrap()
    }

    pub fn fill(db: &mut Database) {
        db.write(|c| {
            c.execute_batch(&format!(
                "INSERT INTO accounts (account_id, institution, name, mask, type, current, available, credit_limit) VALUES
                   ('chk', 'Bank A', 'Checking', '1111', 'depository', 1000, 950, NULL),
                   ('sav', 'Bank A', 'Savings', '2222', 'depository', 5000, 5000, NULL),
                   ('card', 'Card Co', 'Rewards', '3333', 'credit', 300, 700, 1000);
                 INSERT INTO transactions (transaction_id, account_id, date, name, category, detail, amount, pending) VALUES
                   ('t1', 'chk', '2026-11-03', 'Groceries', 'FOOD_AND_DRINK', 'FOOD_AND_DRINK_GROCERIES', 50, 0),
                   ('t2', 'chk', '2026-11-04', 'To savings', 'TRANSFER_OUT', 'TRANSFER_OUT_ACCOUNT_TRANSFER', 1000, 0),
                   ('t3', 'chk', '2026-11-05', 'Card payment', 'LOAN_PAYMENTS', '{CARD_PAYMENT}', 200, 0),
                   ('t4', 'chk', '2026-11-06', 'Payroll', 'INCOME', 'INCOME_WAGES', -2000, 0),
                   ('t5', 'card', '2026-11-07', 'Refund', 'FOOD_AND_DRINK', 'FOOD_AND_DRINK_GROCERIES', -10, 0),
                   ('t6', 'card', '2026-11-10', '<script>alert(1)</script> TV', 'GENERAL_MERCHANDISE', 'GENERAL_MERCHANDISE_ELECTRONICS', 600, 0),
                   ('t7', 'chk', '2026-10-15', 'Rent', 'RENT_AND_UTILITIES', 'RENT_AND_UTILITIES_RENT', 1200, 0);
                 INSERT INTO cards (account_id, statement_balance, statement_date, minimum_payment, due_date)
                   VALUES ('card', 280, '2026-10-20', 25, '2026-11-15');"))
                .map_err(|e| e.to_string())
        })
        .unwrap();
    }

    fn db(t: &TempDir) -> Database {
        let ctx = t.ctx(Env::Production, "correct horse battery staple");
        let mut db = Database::open(&ctx, &mut Secrets::load(&ctx).unwrap()).unwrap();
        fill(&mut db);
        db
    }

    #[test]
    fn balances_and_spending() {
        let t = TempDir::new();
        let db = db(&t);
        let (_, assets, debts) = balances(&db.conn).unwrap();
        assert_eq!((assets, debts), (6000.0, 300.0));
        let m = months(&db.conn, today(), 6).unwrap();
        assert_eq!(m.last().unwrap(), &("2026-11".to_string(), 640.0)); // 50 - 10 + 600: no transfers, income or card payments
        assert_eq!(m[4], ("2026-10".to_string(), 1200.0));
        assert_eq!(
            categories(&db.conn, "2026-11").unwrap()[0],
            ("General merchandise".to_string(), 600.0)
        );
    }

    #[test]
    fn alerts_and_settings() {
        let t = TempDir::new();
        let mut db = db(&t);
        let texts: Vec<String> = alerts(&db.conn, today())
            .unwrap()
            .into_iter()
            .map(|(_, s)| s)
            .collect();
        assert!(
            texts
                .iter()
                .any(|s| s.contains("minimum $25.00 due in 2 days")),
            "{texts:?}"
        );
        assert!(
            texts.iter().any(|s| s.contains("large charge: $600.00")),
            "{texts:?}"
        );
        db.write(|c| {
            c.execute("INSERT INTO settings VALUES ('big_charge', '1000')", [])
                .map_err(|e| e.to_string())
        })
        .unwrap();
        assert!(
            !alerts(&db.conn, today())
                .unwrap()
                .iter()
                .any(|(_, s)| s.contains("large charge"))
        );
    }

    #[test]
    fn search() {
        let t = TempDir::new();
        let db = db(&t);
        let ids = |f: Filter| {
            transactions(&db.conn, &Filter { limit: 50, ..f })
                .unwrap()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(Filter {
                text: Some("food drink".into()),
                ..Filter::default()
            }),
            vec!["Refund", "Groceries"]
        );
        assert_eq!(
            ids(Filter {
                account: Some("3333".into()),
                ..Filter::default()
            })
            .len(),
            2
        );
        assert_eq!(
            ids(Filter {
                since: Some("2026-11-06".into()),
                ..Filter::default()
            })
            .len(),
            3
        );
    }
}
