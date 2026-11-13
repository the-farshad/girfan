//! The dashboard as a web page on this computer, and the page that links banks with Plaid Link.
//!
//! The server listens on 127.0.0.1 only, on a random port, while `girfan web` or `girfan link`
//! runs. It needs the one-time address printed in the terminal, which sets a cookie for the
//! rest of the visit. Requests for any other host name are refused (against DNS rebinding),
//! changes must come from the page itself, and the dashboard pages may load nothing from outside.

use crate::plaid::Plaid;
use crate::report::{self as rp, Filter, Level};
use crate::store::{Ctx, Database, Env, Secrets};
use crate::util::{esc, hex, money, random, same};
use chrono::{Duration, Local, NaiveDate};
use rusqlite::Connection;
use serde_json::json;
use std::io::{Cursor, Read};
use tiny_http::{Header, Method, Request, Response, Server};

const CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; form-action 'self'; base-uri 'none'; frame-ancestors 'none'";
const CSS: &str = "
:root{--bg:#fbfaf7;--fg:#1d1d1f;--mut:#6b6b70;--line:#e3e1db;--acc:#2f6f5e;--warn:#9a6700;--bad:#b42318;--good:#1a7f37;--bar:#7aa89a}
@media (prefers-color-scheme:dark){:root{--bg:#141517;--fg:#e8e6e1;--mut:#9a9890;--line:#2c2d30;--acc:#7cc4ae;--warn:#e3b341;--bad:#ff7b72;--good:#56d364;--bar:#4f7f72}}
*{box-sizing:border-box}body{margin:0;background:var(--bg);color:var(--fg);font:15px/1.45 system-ui,-apple-system,'Segoe UI',sans-serif}
main{max-width:1100px;margin:0 auto;padding:20px 16px 48px}
header{display:flex;flex-wrap:wrap;gap:8px 16px;align-items:baseline;justify-content:space-between}
h1{font-size:22px;margin:0}h2{font-size:13px;text-transform:uppercase;letter-spacing:.07em;color:var(--mut);margin:28px 0 6px}
.mut{color:var(--mut)}.bad{color:var(--bad)}.warn{color:var(--warn)}.good{color:var(--good)}
.wrap{overflow-x:auto}table{width:100%;border-collapse:collapse}
td,th{padding:6px 8px;border-bottom:1px solid var(--line);text-align:left;vertical-align:top}
th{font-weight:600;color:var(--mut);font-size:13px}.n{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.grid{display:grid;grid-template-columns:repeat(auto-fit,minmax(min(100%,340px),1fr));gap:0 32px}
nav{display:flex;flex-wrap:wrap;gap:8px;align-items:center}a{color:var(--acc)}
button,input{font:inherit;color:inherit;background:transparent;border:1px solid var(--line);border-radius:6px;padding:4px 10px}
button{cursor:pointer;color:var(--acc)}form{margin:0}.alerts p{margin:4px 0}.totals{font-size:17px;margin:10px 0}
.flash{border-left:3px solid var(--acc);padding:6px 10px;margin:14px 0;white-space:pre-line}
.search{display:grid;grid-template-columns:repeat(auto-fit,minmax(160px,1fr));gap:8px;margin:14px 0}
svg text{fill:var(--mut);font-size:11px}svg rect{fill:var(--bar)}
";

type Page = Response<Cursor<Vec<u8>>>;

fn reply(code: u16, ctype: &str, body: impl Into<Vec<u8>>, extra: &[(&str, &str)]) -> Page {
    let base = [
        ("Content-Type", ctype),
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
        ("X-Content-Type-Options", "nosniff"),
    ];
    base.iter().chain(extra).fold(
        Response::from_data(body.into()).with_status_code(code),
        |r, (k, v)| {
            r.with_header(Header::from_bytes(k.as_bytes(), v.as_bytes()).expect("an ASCII header"))
        },
    )
}

fn json_reply(v: serde_json::Value) -> Page {
    reply(200, "application/json", v.to_string(), &[])
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hexval = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match (hexval(b[i + 1]), hexval(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h * 16 + l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The path, and the query parameters.
fn split_url(url: &str) -> (String, Vec<(String, String)>) {
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let params = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    (path.to_string(), params)
}

fn param<'a>(q: &'a [(String, String)], key: &str) -> &'a str {
    q.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.trim())
        .unwrap_or("")
}

fn page(title: &str, body: &str, flash: &str) -> String {
    let flash = if flash.is_empty() {
        String::new()
    } else {
        format!("<p class=flash>{}</p>", esc(flash))
    };
    format!(
        "<!doctype html><html lang=en><head><meta charset=utf-8><meta name=viewport content='width=device-width,initial-scale=1'>\
             <title>{}</title><style>{CSS}</style></head><body><main>{flash}{body}</main></body></html>",
        esc(title)
    )
}

const NAV: &str = "<nav><a href='/'>Dashboard</a><a href='/tx'>Transactions</a><a href='/link'>Link a bank</a>\
                   <form method=post action='/sync'><button>Sync now</button></form></nav>";

fn short(x: f64) -> String {
    if x.abs() >= 1000.0 {
        format!("${:.1}k", x / 1000.0)
    } else {
        format!("${x:.0}")
    }
}

fn tx_table(rows: &[rp::Tx]) -> String {
    if rows.is_empty() {
        return "<p class=mut>No transactions.</p>".into();
    }
    let mut out = String::from(
        "<div class=wrap><table><tr><th>Date</th><th>Account</th><th>Description</th><th>Category</th><th class=n>Amount</th></tr>",
    );
    for t in rows {
        let amount = -t.amount;
        let cat = if t.category.is_some() {
            rp::label(t.category.as_deref())
        } else {
            String::new()
        };
        out += &format!(
            "<tr><td class=n>{}</td><td>{}</td><td>{}</td><td class=mut>{}</td><td class='n{}'>{}</td></tr>",
            esc(&t.date),
            esc(&t.account.as_ref().map(|a| a.title()).unwrap_or_default()),
            esc(&t.description()),
            esc(&cat),
            if amount > 0.0 { " good" } else { "" },
            esc(&money(Some(amount), true))
        );
    }
    out + "</table></div>"
}

pub fn dashboard(
    c: &Connection,
    env: Env,
    today: NaiveDate,
    flash: &str,
) -> rusqlite::Result<String> {
    let (last, banks) = rp::last_sync(c)?;
    let mut b = format!(
        "<header><h1>girfan{}</h1>{NAV}</header><p class=mut>{banks} linked bank{}{}</p>",
        if env == Env::Sandbox {
            " <span class=mut>· Sandbox</span>"
        } else {
            ""
        },
        if banks == 1 { "" } else { "s" },
        last.map(|l| format!(
            " · synced {}",
            esc(&l.replace('T', " ").chars().take(16).collect::<String>())
        ))
        .unwrap_or_default()
    );
    let alerts = rp::alerts(c, today)?;
    if !alerts.is_empty() {
        b += "<h2>Alerts</h2><div class=alerts>";
        for (level, text) in &alerts {
            b += &format!(
                "<p class={}>{}</p>",
                if *level == Level::Red { "bad" } else { "warn" },
                esc(text)
            );
        }
        b += "</div>";
    }

    let (accounts, assets, debts) = rp::balances(c)?;
    b += "<h2>Balances</h2>";
    if accounts.is_empty() {
        b += "<p class=mut>No accounts yet. <a href='/link'>Link a bank</a>, or import a statement with <code>girfan import FILE.ofx</code>.</p>";
    } else {
        b += "<div class=wrap><table><tr><th>Bank</th><th>Account</th><th>Type</th><th class=n>Balance</th><th class=n>Available or limit</th></tr>";
        for a in &accounts {
            let debt = rp::is_debt(a.kind.as_deref());
            let other = if debt {
                a.limit.map(|l| format!("{} limit", money(Some(l), false)))
            } else {
                a.available.map(|v| money(Some(v), false))
            };
            b += &format!(
                "<tr><td>{}</td><td>{}</td><td class=mut>{}</td><td class=n>{}{}</td><td class='n mut'>{}</td></tr>",
                esc(a.institution.as_deref().unwrap_or("")),
                esc(&a.short()),
                esc(a.subtype.as_deref().or(a.kind.as_deref()).unwrap_or("")),
                esc(&money(a.current, false)),
                if debt { " owed" } else { "" },
                esc(&other.unwrap_or_default())
            );
        }
        b += &format!(
            "</table></div><p class=totals>You own <b>{}</b> · you owe <b>{}</b> · net worth <b>{}</b></p>",
            esc(&money(Some(assets), false)),
            esc(&money(Some(debts), false)),
            esc(&money(Some(assets - debts), false))
        );
    }

    let cards = rp::cards(c, today)?;
    let due_days = rp::setting(c, "due_days", rp::DUE_DAYS);
    if !cards.is_empty() {
        b += "<h2>Cards</h2><div class=wrap><table><tr><th>Card</th><th class=n>Balance</th><th class=n>Used</th>\
              <th class=n>Statement</th><th class=n>Minimum</th><th>Due</th></tr>";
        for k in &cards {
            let due = k.days.map(|d| {
                format!(
                    "{} ({}){}",
                    k.due_date.as_deref().unwrap_or(""),
                    rp::when(d),
                    if k.paid { " · paid" } else { "" }
                )
            });
            let cls = if k.overdue {
                "bad"
            } else if k.due_soon(due_days) {
                "warn"
            } else {
                "mut"
            };
            b += &format!(
                "<tr><td>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class=n>{}</td><td class={cls}>{}</td></tr>",
                esc(&k.account.title()),
                esc(&money(k.account.current, false)),
                k.used
                    .map(|u| format!("{:.0}%", u * 100.0))
                    .unwrap_or_else(|| "-".into()),
                esc(&money(k.statement_balance, false)),
                esc(&money(k.minimum, false)),
                esc(&due.unwrap_or_default())
            );
        }
        b += "</table></div>";
    }

    let months = rp::months(c, today, 6)?;
    if months.iter().any(|(_, v)| *v != 0.0) {
        let top = months.iter().map(|(_, v)| *v).fold(1.0, f64::max);
        let (w, h, pad) = (560.0, 170.0, 26.0);
        let bw = w / months.len() as f64;
        let mut bars = String::new();
        for (i, (m, v)) in months.iter().enumerate() {
            let bh = (h - 2.0 * pad) * v.max(0.0) / top;
            let (x, y) = (i as f64 * bw + bw * 0.2, h - pad - bh);
            bars += &format!(
                "<rect x='{x:.1}' y='{y:.1}' width='{:.1}' height='{bh:.1}' rx='3'><title>{m}: {}</title></rect>\
                              <text x='{:.1}' y='{:.1}' text-anchor='middle'>{}</text><text x='{:.1}' y='{}' text-anchor='middle'>{m}</text>",
                bw * 0.6,
                esc(&money(Some(*v), false)),
                x + bw * 0.3,
                y - 5.0,
                esc(&short(*v)),
                x + bw * 0.3,
                h - 8.0
            );
        }
        let cats = rp::categories(c, &months.last().expect("six months").0)?;
        b += &format!(
            "<h2>Spending</h2><div class=grid><div><svg viewBox='0 0 {w} {h}' width='100%' role='img' aria-label='Spending per month'>{bars}</svg></div>\
                       <div><table><tr><th>This month by category</th><th class=n>Spent</th></tr>{}</table></div></div>",
            cats.iter()
                .take(10)
                .map(|(k, v)| format!(
                    "<tr><td>{}</td><td class=n>{}</td></tr>",
                    esc(k),
                    esc(&money(Some(*v), false))
                ))
                .collect::<String>()
        );
    }

    b += "<h2>Recent transactions</h2>";
    b += &tx_table(&rp::transactions(
        c,
        &Filter {
            limit: 12,
            ..Filter::default()
        },
    )?);
    b += "<p><a href='/tx'>All transactions</a></p>";
    Ok(page("girfan", &b, flash))
}

pub fn transactions_page(
    c: &Connection,
    today: NaiveDate,
    q: &[(String, String)],
) -> rusqlite::Result<String> {
    let (text, account) = (param(q, "q"), param(q, "account"));
    let days: i64 = param(q, "days").parse().unwrap_or(0).max(0);
    let rows = rp::transactions(
        c,
        &Filter {
            text: Some(text.to_string()),
            account: Some(account.to_string()),
            since: (days > 0).then(|| (today - Duration::days(days)).to_string()),
            min_amount: None,
            limit: 500,
        },
    )?;
    let spent: f64 = rows
        .iter()
        .filter(|t| t.amount > 0.0 && rp::is_spending(t.category.as_deref(), t.detail.as_deref()))
        .map(|t| t.amount)
        .sum();
    let body = format!(
        "<header><h1>Transactions</h1>{NAV}</header><form method=get action='/tx' class=search>\
         <input name=q placeholder='Search description or category' value='{}'><input name=account placeholder='Bank, account or last 4' value='{}'>\
         <input name=days type=number min=0 placeholder='Last N days' value='{}'><button>Search</button></form>\
         <p class=mut>{} shown{} · spending among them {}</p>{}",
        esc(text),
        esc(account),
        if days > 0 {
            days.to_string()
        } else {
            String::new()
        },
        rows.len(),
        if rows.len() == 500 {
            " (the latest 500)"
        } else {
            ""
        },
        esc(&money(Some(spent), false)),
        tx_table(&rows)
    );
    Ok(page("girfan · transactions", &body, ""))
}

/// Plaid Link; this page alone loads Plaid's script, which Plaid requires to come from its site.
pub fn link_page(update: bool) -> String {
    let script = "
const msg = document.getElementById('msg');
const post = (url, body) => fetch(url, {method: 'POST', headers: {'Content-Type': 'application/json'},
  body: JSON.stringify(body || {})}).then(r => r.json());
(async () => {
  const t = await post('/api/link-token');
  if (t.error) { msg.textContent = t.error; return; }
  Plaid.create({
    token: t.link_token,
    onSuccess: async (public_token, metadata) => {
      msg.textContent = 'Linked ' + ((metadata.institution || {}).name || 'the bank') + '. Reading its accounts and transactions…';
      const r = await post(UPDATE ? '/api/updated' : '/api/exchange', {public_token});
      msg.textContent = r.error || r.message;
    },
    onExit: (err) => { msg.textContent = err ? 'Plaid: ' + (err.display_message || err.error_message || err.error_code)
                                             : 'Closed without linking. Reload the page to try again.'; },
  }).open();
})();";
    let body = format!(
        "<header><h1>{}</h1>{NAV}</header><p id=msg class=mut>Opening Plaid…</p>\
                        <p class=mut>Plaid's window opens on this page. Banks with their own login page open it in a pop-up.</p>\
                        <script src='https://cdn.plaid.com/link/v2/stable/link-initialize.js'></script><script>const UPDATE = {update};{script}</script>",
        if update {
            "Log in to a bank again"
        } else {
            "Link a bank"
        }
    );
    page("girfan · link a bank", &body, "")
}

pub struct App<'a> {
    pub ctx: &'a Ctx,
    pub secrets: &'a mut Secrets,
    pub db: &'a mut Database,
    pub update_item: Option<String>,
    pub token: String,
    pub host: String,
    flash: String,
}

fn header<'r>(req: &'r Request, name: &'static str) -> Option<&'r str> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str())
}

impl<'a> App<'a> {
    pub fn new(
        ctx: &'a Ctx,
        secrets: &'a mut Secrets,
        db: &'a mut Database,
        update_item: Option<String>,
        host: String,
        token: String,
    ) -> Self {
        App {
            ctx,
            secrets,
            db,
            update_item,
            token,
            host,
            flash: String::new(),
        }
    }

    pub fn handle(&mut self, mut req: Request) {
        let resp = self.route(&mut req);
        let _ = req.respond(resp);
    }

    fn cookie_ok(&self, req: &Request) -> bool {
        header(req, "Cookie").is_some_and(|c| {
            c.split(';')
                .filter_map(|p| p.trim().strip_prefix("girfan="))
                .any(|v| same(v, &self.token))
        })
    }

    fn sync(&mut self) -> Result<String, String> {
        let plaid = Plaid::new(self.ctx.env, self.secrets)?;
        let mut log = vec![];
        plaid.sync(self.db, self.secrets, &mut |m| log.push(m))?;
        Ok(log.join("\n"))
    }

    fn route(&mut self, req: &mut Request) -> Page {
        let (path, q) = split_url(req.url());
        if header(req, "Host") != Some(self.host.as_str()) {
            return reply(403, "text/plain", "forbidden", &[]);
        }
        let today = Local::now().date_naive();
        match req.method() {
            Method::Get => {
                if same(param(&q, "t"), &self.token) {
                    // the address from the terminal: remember the visit, and drop the code from the address bar
                    let cookie =
                        format!("girfan={}; Path=/; HttpOnly; SameSite=Strict", self.token);
                    return reply(
                        303,
                        "text/plain",
                        "",
                        &[("Location", &path), ("Set-Cookie", &cookie)],
                    );
                }
                if !self.cookie_ok(req) {
                    return reply(
                        403,
                        "text/plain",
                        "Open the address girfan printed in the terminal.",
                        &[],
                    );
                }
                if let Err(e) = self.db.refresh() {
                    return reply(500, "text/plain", e, &[]);
                }
                let html = match path.as_str() {
                    "/" => dashboard(
                        &self.db.conn,
                        self.ctx.env,
                        today,
                        &std::mem::take(&mut self.flash),
                    ),
                    "/tx" => transactions_page(&self.db.conn, today, &q),
                    "/link" => {
                        return reply(
                            200,
                            "text/html; charset=utf-8",
                            link_page(self.update_item.is_some()),
                            &[("Content-Security-Policy", "frame-ancestors 'none'")],
                        );
                    }
                    _ => return reply(404, "text/plain", "not found", &[]),
                };
                match html {
                    Ok(h) => reply(
                        200,
                        "text/html; charset=utf-8",
                        h,
                        &[("Content-Security-Policy", CSP)],
                    ),
                    Err(e) => reply(500, "text/plain", e.to_string(), &[]),
                }
            }
            Method::Post => {
                let origin = format!("http://{}", self.host);
                if !self.cookie_ok(req) || header(req, "Origin") != Some(origin.as_str()) {
                    return reply(403, "text/plain", "forbidden", &[]);
                }
                let json = header(req, "Content-Type")
                    .is_some_and(|t| t.split(';').next() == Some("application/json"));
                let mut body = vec![];
                let _ = req.as_reader().take(10_000).read_to_end(&mut body);
                match path.as_str() {
                    "/sync" => {
                        self.flash = self.sync().unwrap_or_else(|e| e);
                        reply(303, "text/plain", "", &[("Location", "/")])
                    }
                    _ if !json => reply(415, "text/plain", "JSON only", &[]),
                    "/api/link-token" => {
                        let token = self
                            .update_item
                            .as_ref()
                            .and_then(|i| self.secrets.data.tokens.get(i))
                            .cloned();
                        match Plaid::new(self.ctx.env, self.secrets)
                            .and_then(|p| p.link_token(token.as_deref()).map_err(|e| e.to_string()))
                        {
                            Ok(t) => json_reply(json!({"link_token": t})),
                            Err(e) => json_reply(json!({"error": format!("Plaid: {e}")})),
                        }
                    }
                    "/api/exchange" => {
                        let public = serde_json::from_slice::<serde_json::Value>(&body)
                            .ok()
                            .and_then(|v| v["public_token"].as_str().map(String::from));
                        let Some(public) = public else {
                            return json_reply(json!({"error": "no public_token"}));
                        };
                        let linked = Plaid::new(self.ctx.env, self.secrets)
                            .and_then(|p| p.add_item(self.ctx, self.db, self.secrets, &public));
                        match linked {
                            Ok(name) => {
                                let log = self.sync().unwrap_or_else(|e| e);
                                json_reply(
                                    json!({"message": format!("Linked {name}.\n{log}\nYou can link another bank, or go to the dashboard.")}),
                                )
                            }
                            Err(e) => json_reply(json!({"error": e})),
                        }
                    }
                    "/api/updated" => match self.sync() {
                        Ok(log) => {
                            json_reply(json!({"message": format!("Logged in again.\n{log}")}))
                        }
                        Err(e) => json_reply(json!({"error": e})),
                    },
                    _ => reply(404, "text/plain", "not found", &[]),
                }
            }
            _ => reply(405, "text/plain", "method not allowed", &[]),
        }
    }
}

fn open_browser(url: &str) {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(opener)
        .arg(url)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

pub fn serve(
    ctx: &Ctx,
    secrets: &mut Secrets,
    db: &mut Database,
    path: &str,
    update_item: Option<String>,
    open: bool,
) -> Result<(), String> {
    let server = Server::http("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = server
        .server_addr()
        .to_ip()
        .ok_or("the web server has no address")?
        .port();
    let mut app = App::new(
        ctx,
        secrets,
        db,
        update_item,
        format!("127.0.0.1:{port}"),
        hex(&random::<24>()),
    );
    let url = format!("http://{}{path}?t={}", app.host, app.token);
    eprintln!("girfan: open {url}\n(only this computer can reach it; press Ctrl-C here to stop)");
    if open {
        open_browser(&url);
    }
    for req in server.incoming_requests() {
        app.handle(req);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::tests::fill;
    use crate::store::tests::TempDir;
    use std::io::Write;
    use std::net::TcpStream;

    fn send(addr: &str, raw: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(raw.as_bytes()).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn access_checks_escaping_and_search() {
        let t = TempDir::new();
        let ctx = t.ctx(Env::Sandbox, "");
        let mut secrets = Secrets::load(&ctx).unwrap();
        let mut db = Database::open(&ctx, &mut secrets).unwrap();
        fill(&mut db);
        let server = Server::http("127.0.0.1:0").unwrap();
        let addr = server.server_addr().to_ip().unwrap().to_string();
        let token = "test-token-0123456789".to_string();
        let get = |path: &str, extra: &str| {
            format!("GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n{extra}\r\n")
        };
        let cookie = format!("Cookie: girfan={token}\r\n");
        let post = |extra: &str| {
            format!(
                "POST /sync HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Length: 0\r\n{cookie}{extra}\r\n"
            )
        };
        let requests = [
            get("/", ""),
            get(&format!("/?t={token}"), ""),
            get("/", &cookie),
            format!("GET / HTTP/1.1\r\nHost: evil.example\r\nConnection: close\r\n{cookie}\r\n"),
            post(""),
            post("Origin: http://evil.example\r\n"),
            get("/tx?q=rent", &cookie),
        ];
        let answers: Vec<String> = std::thread::scope(|s| {
            s.spawn(|| {
                let mut app = App::new(
                    &ctx,
                    &mut secrets,
                    &mut db,
                    None,
                    addr.clone(),
                    token.clone(),
                );
                for _ in 0..requests.len() {
                    app.handle(server.recv().unwrap());
                }
            });
            requests.iter().map(|r| send(&addr, r)).collect()
        });
        let status = |i: usize| answers[i].split_whitespace().nth(1).unwrap().to_string();
        assert_eq!(status(0), "403");
        assert_eq!(status(1), "303");
        assert!(answers[1].contains("HttpOnly; SameSite=Strict"));
        assert_eq!(status(2), "200");
        assert!(answers[2].contains("default-src 'none'") && answers[2].contains("net worth"));
        assert!(!answers[2].contains("<script>alert") && answers[2].contains("&lt;script&gt;")); // names are escaped
        assert_eq!(
            (status(3), status(4), status(5)),
            ("403".into(), "403".into(), "403".into())
        );
        assert_eq!(status(6), "200");
        assert!(answers[6].contains("Rent") && !answers[6].contains("Groceries"));
    }

    #[test]
    fn query_strings() {
        let (path, q) = split_url("/tx?q=coffee+shop&account=%C2%B7%C2%B71234&days=7");
        assert_eq!(path, "/tx");
        assert_eq!(
            (param(&q, "q"), param(&q, "account"), param(&q, "days")),
            ("coffee shop", "··1234", "7")
        );
    }
}
