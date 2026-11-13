//! Plaid: linking banks, and reading their accounts, transactions and credit cards.
//!
//! Plaid's REST API is called directly (there is no official Rust library). Only read-only
//! products are used: Transactions, and Liabilities (card due dates and minimum payments)
//! where the bank offers it. Products that return full account and routing numbers (Auth,
//! Identity) are never requested.

use crate::store::{Database, Env, Keys, Secrets};
use chrono::Local;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::fmt;
use std::time::Duration;

pub const TEST_BANK: &str = "ins_109508"; // First Platypus Bank, Plaid's Sandbox test institution
const NO_CARDS: &[&str] = &[
    "PRODUCTS_NOT_SUPPORTED",
    "NO_LIABILITY_ACCOUNTS",
    "ADDITIONAL_CONSENT_REQUIRED",
    "PRODUCT_NOT_READY",
    "INVALID_PRODUCT",
    "PRODUCT_NOT_ENABLED",
];

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match (self.code.is_empty(), self.status) {
            (false, _) => write!(f, "{}", self.code),
            (true, 0) => write!(f, "could not reach Plaid: {}", self.message),
            (true, s) => write!(f, "HTTP {s}: {}", self.message),
        }
    }
}

fn local(message: impl fmt::Display) -> ApiError {
    ApiError {
        status: 0,
        code: "LOCAL".into(),
        message: message.to_string(),
    }
}

pub fn now() -> String {
    Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
}

fn text(v: &Value) -> Option<String> {
    v.as_str().filter(|s| !s.is_empty()).map(String::from)
}

pub struct Plaid {
    agent: ureq::Agent,
    host: &'static str,
    keys: Keys,
}

impl Plaid {
    pub fn new(env: Env, secrets: &Secrets) -> Result<Self, String> {
        let keys = secrets
            .data
            .keys
            .clone()
            .ok_or_else(|| format!("no Plaid keys yet; run: girfan {}keys", env.flag()))?;
        Ok(Plaid {
            agent: Self::agent(),
            host: env.host(),
            keys,
        })
    }

    pub fn with_keys(env: Env, keys: Keys) -> Self {
        Plaid {
            agent: Self::agent(),
            host: env.host(),
            keys,
        }
    }

    fn agent() -> ureq::Agent {
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(90)))
            .build()
            .into()
    }

    fn call(&self, path: &str, mut body: Value) -> Result<Value, ApiError> {
        body["client_id"] = json!(self.keys.client_id);
        body["secret"] = json!(self.keys.secret);
        let mut resp = self
            .agent
            .post(format!("{}{path}", self.host))
            .header("Plaid-Version", "2020-09-14")
            .send_json(&body)
            .map_err(|e| ApiError {
                status: 0,
                code: String::new(),
                message: e.to_string(),
            })?;
        let status = resp.status().as_u16();
        let v: Value = resp.body_mut().read_json().map_err(|e| ApiError {
            status,
            code: String::new(),
            message: e.to_string(),
        })?;
        if status == 200 {
            Ok(v)
        } else {
            Err(ApiError {
                status,
                code: text(&v["error_code"]).unwrap_or_default(),
                message: text(&v["error_message"]).unwrap_or_default(),
            })
        }
    }

    /// A token for Plaid Link: to add a bank, or (with ACCESS_TOKEN) to log in to one again.
    pub fn link_token(&self, access_token: Option<&str>) -> Result<String, ApiError> {
        let mut body = json!({"client_name": "girfan", "language": "en", "country_codes": ["US"],
                              "user": {"client_user_id": "girfan"}});
        match access_token {
            Some(t) => body["access_token"] = json!(t),
            None => {
                body["products"] = json!(["transactions"]);
                body["optional_products"] = json!(["liabilities"]);
                body["transactions"] = json!({"days_requested": 730});
            }
        }
        let r = self.call("/link/token/create", body)?;
        text(&r["link_token"]).ok_or_else(|| local("no link_token in Plaid's answer"))
    }

    /// Stores a newly linked bank: its token in the secrets, the rest in the database.
    pub fn add_item(
        &self,
        ctx: &crate::store::Ctx,
        db: &mut Database,
        secrets: &mut Secrets,
        public_token: &str,
    ) -> Result<String, String> {
        let r = self
            .call(
                "/item/public_token/exchange",
                json!({"public_token": public_token}),
            )
            .map_err(|e| e.to_string())?;
        let (Some(token), Some(item_id)) = (text(&r["access_token"]), text(&r["item_id"])) else {
            return Err("Plaid's answer had no access token".into());
        };
        secrets.data.tokens.insert(item_id.clone(), token.clone());
        secrets.save(ctx)?;
        let inst_id = self
            .call("/item/get", json!({"access_token": token}))
            .ok()
            .and_then(|r| text(&r["item"]["institution_id"]));
        let name = inst_id
            .as_deref()
            .and_then(|id| {
                self.call(
                    "/institutions/get_by_id",
                    json!({"institution_id": id, "country_codes": ["US"]}),
                )
                .ok()
            })
            .and_then(|r| text(&r["institution"]["name"]))
            .or_else(|| inst_id.clone())
            .unwrap_or_else(|| "unknown bank".into());
        db.write(|c| {
            c.execute("INSERT OR REPLACE INTO items (item_id, institution_id, institution, added) VALUES (?1, ?2, ?3, ?4)",
                      params![item_id, inst_id, name, now()]).map_err(|e| e.to_string())
        })?;
        Ok(name)
    }

    /// Links Plaid's Sandbox test bank without a browser.
    pub fn sandbox_item(
        &self,
        ctx: &crate::store::Ctx,
        db: &mut Database,
        secrets: &mut Secrets,
    ) -> Result<String, String> {
        let mut last = String::new();
        for products in [
            json!(["transactions", "liabilities"]),
            json!(["transactions"]),
        ] {
            match self.call(
                "/sandbox/public_token/create",
                json!({"institution_id": TEST_BANK, "initial_products": products}),
            ) {
                Ok(r) => {
                    let public =
                        text(&r["public_token"]).ok_or("no public token in Plaid's answer")?;
                    return self.add_item(ctx, db, secrets, &public);
                }
                Err(e) => last = e.to_string(),
            }
        }
        Err(last)
    }

    /// Institutions matching QUERY that support Transactions: (name, id, uses its own login page).
    pub fn search(&self, query: &str) -> Result<Vec<(String, String, bool)>, ApiError> {
        let r = self.call(
            "/institutions/search",
            json!({"query": query, "products": ["transactions"], "country_codes": ["US"]}),
        )?;
        Ok(r["institutions"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|i| {
                (
                    text(&i["name"]).unwrap_or_default(),
                    text(&i["institution_id"]).unwrap_or_default(),
                    i["oauth"].as_bool().unwrap_or(false),
                )
            })
            .collect())
    }

    /// Fetches new transactions, balances and card details for every linked bank.
    pub fn sync(
        &self,
        db: &mut Database,
        secrets: &Secrets,
        log: &mut dyn FnMut(String),
    ) -> Result<(), String> {
        db.refresh()?;
        let items: Vec<(String, String)> = {
            let mut st = db
                .conn
                .prepare("SELECT item_id, institution FROM items ORDER BY institution")
                .map_err(|e| e.to_string())?;
            st.query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            })
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect()
        };
        if items.is_empty() {
            log("no linked banks yet".into());
        }
        for (item_id, inst) in items {
            let Some(token) = secrets.data.tokens.get(&item_id) else {
                log(format!("{inst}: no access token stored; link it again"));
                continue;
            };
            let msg = db.write(|c| {
                Ok(match self.sync_item(c, &item_id, &inst, token) {
                    Ok(m) => {
                        c.execute(
                            "UPDATE items SET synced = ?1, error = NULL WHERE item_id = ?2",
                            params![now(), item_id],
                        )
                        .map_err(|e| e.to_string())?;
                        m
                    }
                    Err(e) => {
                        c.execute(
                            "UPDATE items SET error = ?1 WHERE item_id = ?2",
                            params![e.to_string(), item_id],
                        )
                        .map_err(|e| e.to_string())?;
                        let hint = if e.code == "ITEM_LOGIN_REQUIRED" {
                            format!(
                                "; log in again with: girfan link --update {}",
                                &item_id[..8.min(item_id.len())]
                            )
                        } else {
                            String::new()
                        };
                        format!("{inst}: {e}{hint}")
                    }
                })
            })?;
            log(msg);
        }
        Ok(())
    }

    fn sync_item(
        &self,
        c: &Connection,
        item_id: &str,
        inst: &str,
        token: &str,
    ) -> Result<String, ApiError> {
        let start: Option<String> = c
            .query_row(
                "SELECT cursor FROM items WHERE item_id = ?1",
                [item_id],
                |r| r.get(0),
            )
            .map_err(local)?;
        let (mut cursor, mut counts, mut restarts) = (start.clone(), [0usize; 3], 0);
        let status;
        loop {
            let mut body = json!({"access_token": token, "count": 500});
            if let Some(cur) = &cursor {
                body["cursor"] = json!(cur);
            }
            let r = match self.call("/transactions/sync", body) {
                Ok(r) => r,
                Err(e)
                    if e.code == "TRANSACTIONS_SYNC_MUTATION_DURING_PAGINATION" && restarts < 3 =>
                {
                    (cursor, restarts) = (start.clone(), restarts + 1); // the data changed while paging: start over
                    continue;
                }
                Err(e) => return Err(e),
            };
            for (i, key) in ["added", "modified"].iter().enumerate() {
                for t in r[*key].as_array().into_iter().flatten() {
                    put_transaction(c, t).map_err(local)?;
                    counts[i] += 1;
                }
            }
            for t in r["removed"].as_array().into_iter().flatten() {
                c.execute(
                    "DELETE FROM transactions WHERE transaction_id = ?1",
                    [t["transaction_id"].as_str()],
                )
                .map_err(local)?;
                counts[2] += 1;
            }
            cursor = text(&r["next_cursor"]);
            if !r["has_more"].as_bool().unwrap_or(false) {
                status = text(&r["transactions_update_status"]).unwrap_or_default();
                break;
            }
        }
        c.execute(
            "UPDATE items SET cursor = ?1 WHERE item_id = ?2",
            params![cursor, item_id],
        )
        .map_err(local)?;

        let r = self.call("/accounts/get", json!({"access_token": token}))?;
        for a in r["accounts"].as_array().into_iter().flatten() {
            let b = &a["balances"];
            c.execute("INSERT OR REPLACE INTO accounts (account_id, item_id, institution, name, mask, type, subtype, current, \
                       available, credit_limit, currency, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                      params![text(&a["account_id"]), item_id, inst, text(&a["name"]).or_else(|| text(&a["official_name"])),
                              text(&a["mask"]), text(&a["type"]), text(&a["subtype"]), b["current"].as_f64(),
                              b["available"].as_f64(), b["limit"].as_f64(),
                              text(&b["iso_currency_code"]).or_else(|| text(&b["unofficial_currency_code"])), now()])
                .map_err(local)?;
        }

        let (cards, note) = match self.call("/liabilities/get", json!({"access_token": token})) {
            Ok(r) => {
                let credit = r["liabilities"]["credit"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                for k in &credit {
                    c.execute("INSERT OR REPLACE INTO cards (account_id, statement_balance, statement_date, minimum_payment, \
                               due_date, last_payment, last_payment_date, overdue, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                              params![text(&k["account_id"]), k["last_statement_balance"].as_f64(), text(&k["last_statement_issue_date"]),
                                      k["minimum_payment_amount"].as_f64(), text(&k["next_payment_due_date"]),
                                      k["last_payment_amount"].as_f64(), text(&k["last_payment_date"]),
                                      k["is_overdue"].as_bool().unwrap_or(false), now()])
                        .map_err(local)?;
                }
                (credit.len(), None)
            }
            Err(e) if NO_CARDS.contains(&e.code.as_str()) => (0, None),
            Err(e) => (0, Some(format!("card details: {e}"))),
        };

        let mut msg = format!(
            "{inst}: {} new, {} changed, {} removed transactions",
            counts[0], counts[1], counts[2]
        );
        if cards > 0 {
            msg += &format!(", {cards} card{}", if cards == 1 { "" } else { "s" });
        }
        if let Some(n) = note {
            msg += &format!(" ({n})");
        }
        if status == "NOT_READY" {
            msg += "; Plaid is still preparing the history, so sync again in a few minutes";
        }
        Ok(msg)
    }

    /// Ends girfan's access to a bank at Plaid and forgets its data here.
    pub fn remove(
        &self,
        ctx: &crate::store::Ctx,
        db: &mut Database,
        secrets: &mut Secrets,
        item_id: &str,
    ) -> Result<(), String> {
        if let Some(token) = secrets.data.tokens.get(item_id).cloned() {
            self.call("/item/remove", json!({"access_token": token}))
                .map_err(|e| e.to_string())?;
            secrets.data.tokens.remove(item_id);
            secrets.save(ctx)?;
        }
        db.write(|c| {
            for table in ["transactions", "cards"] {
                c.execute(&format!("DELETE FROM {table} WHERE account_id IN (SELECT account_id FROM accounts WHERE item_id = ?1)"),
                          [item_id]).map_err(|e| e.to_string())?;
            }
            c.execute("DELETE FROM accounts WHERE item_id = ?1", [item_id]).map_err(|e| e.to_string())?;
            c.execute("DELETE FROM items WHERE item_id = ?1", [item_id]).map_err(|e| e.to_string())?;
            Ok(())
        })
    }
}

fn put_transaction(c: &Connection, t: &Value) -> rusqlite::Result<usize> {
    let pfc = &t["personal_finance_category"];
    c.execute("INSERT OR REPLACE INTO transactions (transaction_id, account_id, date, name, merchant, amount, currency, \
               category, detail, pending) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
              params![text(&t["transaction_id"]), text(&t["account_id"]), text(&t["date"]), text(&t["name"]).unwrap_or_default(),
                      text(&t["merchant_name"]), t["amount"].as_f64().unwrap_or(0.0),
                      text(&t["iso_currency_code"]).or_else(|| text(&t["unofficial_currency_code"])),
                      text(&pfc["primary"]), text(&pfc["detailed"]), t["pending"].as_bool().unwrap_or(false)])
}
